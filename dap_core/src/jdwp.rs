/// Minimal async JDWP client covering the commands needed to launch, connect,
/// set breakpoints, and inspect JVM state.
///
/// Assumes 8-byte IDs (the default on all modern 64-bit JVMs).
/// ID size is validated via `VirtualMachine.IDSizes` at connection time; a
/// warning is logged if non-8-byte IDs are reported.
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Result, anyhow};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const HANDSHAKE: &[u8] = b"JDWP-Handshake";
// All IDs are 8 bytes on modern 64-bit JVMs (validated at connect time via IDSizes).

pub const EVENT_STEP: u8 = 1;
pub const EVENT_BREAKPOINT: u8 = 2;
pub const EVENT_EXCEPTION: u8 = 4;
pub const EVENT_CLASS_PREPARE: u8 = 8;
pub const EVENT_VM_START: u8 = 90;
pub const EVENT_VM_DEATH: u8 = 99;

pub const STEP_SIZE_LINE: u32 = 1;
pub const STEP_DEPTH_INTO: u32 = 0;
pub const STEP_DEPTH_OVER: u32 = 1;
pub const STEP_DEPTH_OUT: u32 = 2;

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Location {
    pub type_tag: u8,
    pub class_id: u64,
    pub method_id: u64,
    pub index: u64,
}

#[derive(Debug, Clone)]
pub struct ClassInfo {
    pub ref_type_tag: u8,
    pub ref_type_id: u64,
    pub signature: String,
    pub status: u32,
}

#[derive(Debug, Clone)]
pub struct MethodInfo {
    pub method_id: u64,
    pub name: String,
    pub signature: String,
    pub mod_bits: u32,
}

#[derive(Debug, Clone)]
pub struct LineTableEntry {
    pub line_code_index: u64,
    pub line_number: u32,
}

#[derive(Debug, Clone)]
pub struct LineTable {
    pub start: u64,
    pub end: u64,
    pub lines: Vec<LineTableEntry>,
}

#[derive(Debug, Clone)]
pub struct VariableInfo {
    pub code_index: u64,
    pub name: String,
    pub signature: String,
    pub length: u32,
    pub slot: u32,
}

#[derive(Debug, Clone)]
pub struct FrameInfo {
    pub frame_id: u64,
    pub location: Location,
}

#[derive(Debug, Clone)]
pub enum JvmValue {
    Byte(u8),
    Char(u16),
    Double(f64),
    Float(f32),
    Int(u32),
    Long(u64),
    Short(u16),
    Boolean(bool),
    Object { tag: u8, id: u64 },
    Void,
}

impl JvmValue {
    pub fn display(&self) -> String {
        match self {
            JvmValue::Byte(v) => v.to_string(),
            JvmValue::Char(v) => {
                char::from_u32(*v as u32)
                    .map(|c| format!("'{c}'"))
                    .unwrap_or_else(|| format!("'\\u{v:04x}'"))
            }
            JvmValue::Double(v) => v.to_string(),
            JvmValue::Float(v) => v.to_string(),
            JvmValue::Int(v) => (*v as i32).to_string(),
            JvmValue::Long(v) => (*v as i64).to_string(),
            JvmValue::Short(v) => (*v as i16).to_string(),
            JvmValue::Boolean(v) => v.to_string(),
            JvmValue::Object { id, .. } => format!("@{id:#x}"),
            JvmValue::Void => "void".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Event types
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct ClassPrepareEvent {
    pub request_id: u32,
    pub thread_id: u64,
    pub ref_type_tag: u8,
    pub ref_type_id: u64,
    pub signature: String,
    pub status: u32,
}

#[derive(Debug)]
pub struct BreakpointEvent {
    pub request_id: u32,
    pub thread_id: u64,
    pub location: Location,
}

#[derive(Debug)]
pub struct StepEvent {
    pub request_id: u32,
    pub thread_id: u64,
    pub location: Location,
}

#[derive(Debug)]
pub struct ExceptionUncaughtEvent {
    pub request_id: u32,
    pub thread_id: u64,
    /// Object ID of the exception instance.
    pub exception_object_id: u64,
}

#[derive(Debug)]
pub enum JdwpEvent {
    ClassPrepare(ClassPrepareEvent),
    Breakpoint(BreakpointEvent),
    Step(StepEvent),
    /// An uncaught exception was thrown; JVM is suspended.
    ExceptionUncaught(ExceptionUncaughtEvent),
    /// JVM has terminated or the JDWP connection was closed.
    VmDeath,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

type PendingMap = Arc<Mutex<HashMap<u32, oneshot::Sender<(u16, Vec<u8>)>>>>;

pub struct JdwpClient {
    write_tx: mpsc::UnboundedSender<Vec<u8>>,
    pending: PendingMap,
    next_id: Arc<AtomicU32>,
}

impl JdwpClient {
    /// Connects to a JDWP endpoint at `addr`, performs the handshake, validates
    /// ID sizes, and returns the client plus a receiver for incoming VM events.
    pub async fn connect(addr: &str) -> Result<(Self, mpsc::UnboundedReceiver<JdwpEvent>)> {
        let stream = TcpStream::connect(addr)
            .await
            .map_err(|e| anyhow!("JDWP TCP connect to {addr} failed: {e}"))?;
        let (mut reader, mut writer) = stream.into_split();

        // JDWP handshake: both sides send/receive the 14-byte magic string.
        writer.write_all(HANDSHAKE).await?;
        let mut buf = vec![0u8; HANDSHAKE.len()];
        reader.read_exact(&mut buf).await?;
        if buf != HANDSHAKE {
            return Err(anyhow!("JDWP handshake mismatch"));
        }
        debug!("JDWP handshake complete with {addr}");

        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<JdwpEvent>();
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));

        // Write loop: drains the write channel onto the TCP socket.
        tokio::spawn(async move {
            while let Some(bytes) = write_rx.recv().await {
                if writer.write_all(&bytes).await.is_err() {
                    break;
                }
            }
        });

        // Read loop: dispatches reply packets to waiting callers and events to
        // the event channel.
        let pending_clone = pending.clone();
        tokio::spawn(async move {
            loop {
                let mut header = [0u8; 11];
                if reader.read_exact(&mut header).await.is_err() {
                    break;
                }
                let length = u32::from_be_bytes(header[0..4].try_into().unwrap()) as usize;
                let id = u32::from_be_bytes(header[4..8].try_into().unwrap());
                let flags = header[8];

                let data_len = length.saturating_sub(11);
                let mut data = vec![0u8; data_len];
                if data_len > 0 && reader.read_exact(&mut data).await.is_err() {
                    break;
                }

                if flags & 0x80 != 0 {
                    // Reply packet: bytes 9-10 are the error code.
                    let error_code = u16::from_be_bytes([header[9], header[10]]);
                    let mut guard = pending_clone.lock().await;
                    if let Some(sender) = guard.remove(&id) {
                        let _ = sender.send((error_code, data));
                    }
                } else {
                    // Command packet from JVM — always an event composite.
                    let cmd_set = header[9];
                    let cmd = header[10];
                    if cmd_set == 64 && cmd == 100 {
                        match parse_composite_event(&data) {
                            Ok(events) => {
                                for ev in events {
                                    if event_tx.send(ev).is_err() {
                                        return;
                                    }
                                }
                            }
                            Err(e) => warn!("failed to parse JDWP composite event: {e}"),
                        }
                    }
                }
            }
            // Connection closed. Clear all pending reply senders first so that
            // any `send_command` calls that are awaiting a reply unblock with an
            // error instead of blocking forever (which would cause a deadlock if
            // the caller holds a lock that the event consumer also needs).
            pending_clone.lock().await.clear();
            // Signal termination to the consumer.
            let _ = event_tx.send(JdwpEvent::VmDeath);
        });

        let client = JdwpClient {
            write_tx,
            pending,
            next_id: Arc::new(AtomicU32::new(1)),
        };

        // Validate ID sizes: warn if the JVM reports non-8-byte IDs.
        match client.vm_id_sizes().await {
            Ok((field, method, object, ref_type, frame)) => {
                if object != 8 || ref_type != 8 || method != 8 || field != 8 || frame != 8 {
                    warn!(
                        "JVM ID sizes are not 8 bytes (object={object}, refType={ref_type}, \
                         method={method}, field={field}, frame={frame}); \
                         dapintar assumes 8-byte IDs and may behave incorrectly"
                    );
                }
            }
            Err(e) => warn!("failed to query JDWP ID sizes: {e}"),
        }

        Ok((client, event_rx))
    }

    // -----------------------------------------------------------------------
    // Internal: packet send + reply receive
    // -----------------------------------------------------------------------

    async fn send_command(&self, cmd_set: u8, cmd: u8, data: Vec<u8>) -> Result<Vec<u8>> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let length = (11 + data.len()) as u32;
        let mut packet = Vec::with_capacity(11 + data.len());
        packet.extend_from_slice(&length.to_be_bytes());
        packet.extend_from_slice(&id.to_be_bytes());
        packet.push(0x00); // flags = command
        packet.push(cmd_set);
        packet.push(cmd);
        packet.extend_from_slice(&data);

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        self.write_tx
            .send(packet)
            .map_err(|_| anyhow!("JDWP write channel closed"))?;

        let (error_code, reply) = rx
            .await
            .map_err(|_| anyhow!("JDWP reply channel dropped (JVM exited?)"))?;
        if error_code != 0 {
            return Err(anyhow!("JDWP error code {error_code} (cmdSet={cmd_set} cmd={cmd})"));
        }
        Ok(reply)
    }

    // -----------------------------------------------------------------------
    // VirtualMachine commands (command set 1)
    // -----------------------------------------------------------------------

    pub async fn vm_version(&self) -> Result<String> {
        let data = self.send_command(1, 1, vec![]).await?;
        let mut pos = 0;
        read_string(&data, &mut pos)
    }

    /// Returns (fieldIDSize, methodIDSize, objectIDSize, referenceTypeIDSize, frameIDSize).
    pub async fn vm_id_sizes(&self) -> Result<(usize, usize, usize, usize, usize)> {
        let data = self.send_command(1, 7, vec![]).await?;
        let mut pos = 0;
        let field = read_u32(&data, &mut pos)? as usize;
        let method = read_u32(&data, &mut pos)? as usize;
        let object = read_u32(&data, &mut pos)? as usize;
        let ref_type = read_u32(&data, &mut pos)? as usize;
        let frame = read_u32(&data, &mut pos)? as usize;
        Ok((field, method, object, ref_type, frame))
    }

    /// AllClassesWithGeneric — returns every class currently loaded in the JVM.
    pub async fn vm_all_classes(&self) -> Result<Vec<ClassInfo>> {
        let data = self.send_command(1, 20, vec![]).await?;
        let mut pos = 0;
        let count = read_u32(&data, &mut pos)? as usize;
        let mut classes = Vec::with_capacity(count);
        for _ in 0..count {
            let ref_type_tag = read_u8(&data, &mut pos)?;
            let ref_type_id = read_id(&data, &mut pos)?;
            let signature = read_string(&data, &mut pos)?;
            let _generic = read_string(&data, &mut pos)?;
            let status = read_u32(&data, &mut pos)?;
            classes.push(ClassInfo { ref_type_tag, ref_type_id, signature, status });
        }
        Ok(classes)
    }

    pub async fn vm_all_threads(&self) -> Result<Vec<u64>> {
        let data = self.send_command(1, 4, vec![]).await?;
        let mut pos = 0;
        let count = read_u32(&data, &mut pos)? as usize;
        let mut threads = Vec::with_capacity(count);
        for _ in 0..count {
            threads.push(read_id(&data, &mut pos)?);
        }
        Ok(threads)
    }

    pub async fn vm_resume(&self) -> Result<()> {
        self.send_command(1, 9, vec![]).await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // ReferenceType commands (command set 2)
    // -----------------------------------------------------------------------

    pub async fn ref_type_name(&self, ref_type_id: u64) -> Result<String> {
        let mut req = Vec::new();
        write_id(&mut req, ref_type_id);
        let data = self.send_command(2, 1, req).await?;
        let mut pos = 0;
        read_string(&data, &mut pos)
    }

    /// MethodsWithGeneric — returns all methods declared by a reference type.
    pub async fn ref_type_methods(&self, ref_type_id: u64) -> Result<Vec<MethodInfo>> {
        let mut req = Vec::new();
        write_id(&mut req, ref_type_id);
        let data = self.send_command(2, 15, req).await?;
        let mut pos = 0;
        let count = read_u32(&data, &mut pos)? as usize;
        let mut methods = Vec::with_capacity(count);
        for _ in 0..count {
            let method_id = read_id(&data, &mut pos)?;
            let name = read_string(&data, &mut pos)?;
            let signature = read_string(&data, &mut pos)?;
            let _generic = read_string(&data, &mut pos)?;
            let mod_bits = read_u32(&data, &mut pos)?;
            methods.push(MethodInfo { method_id, name, signature, mod_bits });
        }
        Ok(methods)
    }

    // -----------------------------------------------------------------------
    // Method commands (command set 6)
    // -----------------------------------------------------------------------

    pub async fn method_line_table(&self, ref_type_id: u64, method_id: u64) -> Result<LineTable> {
        let mut req = Vec::new();
        write_id(&mut req, ref_type_id);
        write_id(&mut req, method_id);
        let data = self.send_command(6, 1, req).await?;
        let mut pos = 0;
        let start = read_u64(&data, &mut pos)?;
        let end = read_u64(&data, &mut pos)?;
        let count = read_u32(&data, &mut pos)? as usize;
        let mut lines = Vec::with_capacity(count);
        for _ in 0..count {
            let line_code_index = read_u64(&data, &mut pos)?;
            let line_number = read_u32(&data, &mut pos)?;
            lines.push(LineTableEntry { line_code_index, line_number });
        }
        Ok(LineTable { start, end, lines })
    }

    /// VariableTableWithGeneric — requires the class was compiled with debug info.
    /// Returns `Err` with JDWP error 101 (ABSENT_INFORMATION) if debug info is missing.
    pub async fn method_variable_table(
        &self,
        ref_type_id: u64,
        method_id: u64,
    ) -> Result<Vec<VariableInfo>> {
        let mut req = Vec::new();
        write_id(&mut req, ref_type_id);
        write_id(&mut req, method_id);
        let data = self.send_command(6, 5, req).await?;
        let mut pos = 0;
        let _arg_cnt = read_u32(&data, &mut pos)?;
        let count = read_u32(&data, &mut pos)? as usize;
        let mut vars = Vec::with_capacity(count);
        for _ in 0..count {
            let code_index = read_u64(&data, &mut pos)?;
            let name = read_string(&data, &mut pos)?;
            let signature = read_string(&data, &mut pos)?;
            let _generic = read_string(&data, &mut pos)?;
            let length = read_u32(&data, &mut pos)?;
            let slot = read_u32(&data, &mut pos)?;
            vars.push(VariableInfo { code_index, name, signature, length, slot });
        }
        Ok(vars)
    }

    // -----------------------------------------------------------------------
    // ThreadReference commands (command set 11)
    // -----------------------------------------------------------------------

    pub async fn thread_name(&self, thread_id: u64) -> Result<String> {
        let mut req = Vec::new();
        write_id(&mut req, thread_id);
        let data = self.send_command(11, 1, req).await?;
        let mut pos = 0;
        read_string(&data, &mut pos)
    }

    pub async fn thread_resume(&self, thread_id: u64) -> Result<()> {
        let mut req = Vec::new();
        write_id(&mut req, thread_id);
        self.send_command(11, 3, req).await?;
        Ok(())
    }

    /// Returns all frames for `thread_id`. Pass `length = -1i32 as u32` for all frames.
    pub async fn thread_frames(
        &self,
        thread_id: u64,
        start_frame: u32,
        length: u32,
    ) -> Result<Vec<FrameInfo>> {
        let mut req = Vec::new();
        write_id(&mut req, thread_id);
        req.extend_from_slice(&start_frame.to_be_bytes());
        req.extend_from_slice(&length.to_be_bytes());
        let data = self.send_command(11, 6, req).await?;
        let mut pos = 0;
        let count = read_u32(&data, &mut pos)? as usize;
        let mut frames = Vec::with_capacity(count);
        for _ in 0..count {
            let frame_id = read_id(&data, &mut pos)?;
            let type_tag = read_u8(&data, &mut pos)?;
            let class_id = read_id(&data, &mut pos)?;
            let method_id = read_id(&data, &mut pos)?;
            let index = read_u64(&data, &mut pos)?;
            frames.push(FrameInfo {
                frame_id,
                location: Location { type_tag, class_id, method_id, index },
            });
        }
        Ok(frames)
    }

    // -----------------------------------------------------------------------
    // StackFrame commands (command set 16)
    // -----------------------------------------------------------------------

    /// Reads values from local variable slots. Each `(slot, tag)` pair specifies
    /// the slot index and the expected JDWP type tag (e.g. `b'I'` for int).
    pub async fn stack_frame_get_values(
        &self,
        thread_id: u64,
        frame_id: u64,
        slots: &[(u32, u8)],
    ) -> Result<Vec<JvmValue>> {
        let mut req = Vec::new();
        write_id(&mut req, thread_id);
        write_id(&mut req, frame_id);
        req.extend_from_slice(&(slots.len() as u32).to_be_bytes());
        for (slot, tag) in slots {
            req.extend_from_slice(&slot.to_be_bytes());
            req.push(*tag);
        }
        let data = self.send_command(16, 1, req).await?;
        let mut pos = 0;
        let count = read_u32(&data, &mut pos)? as usize;
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            values.push(read_tagged_value(&data, &mut pos)?);
        }
        Ok(values)
    }

    // -----------------------------------------------------------------------
    // EventRequest commands (command set 15)
    // -----------------------------------------------------------------------

    /// Registers a ClassPrepare listener that suspends all threads.
    /// Returns the assigned request ID (needed for `event_request_clear`).
    pub async fn event_request_set_class_prepare(&self) -> Result<u32> {
        let mut req = Vec::new();
        req.push(EVENT_CLASS_PREPARE);
        req.push(2u8); // SUSPEND_ALL
        req.extend_from_slice(&0u32.to_be_bytes()); // 0 modifiers
        let data = self.send_command(15, 1, req).await?;
        let mut pos = 0;
        read_u32(&data, &mut pos)
    }

    /// Installs a breakpoint at `location`. Suspends all threads on hit.
    pub async fn event_request_set_breakpoint(&self, location: &Location) -> Result<u32> {
        let mut req = Vec::new();
        req.push(EVENT_BREAKPOINT);
        req.push(2u8); // SUSPEND_ALL
        req.extend_from_slice(&1u32.to_be_bytes()); // 1 modifier
        req.push(7u8); // LocationOnly modifier
        req.push(location.type_tag);
        write_id(&mut req, location.class_id);
        write_id(&mut req, location.method_id);
        req.extend_from_slice(&location.index.to_be_bytes());
        let data = self.send_command(15, 1, req).await?;
        let mut pos = 0;
        read_u32(&data, &mut pos)
    }

    /// Registers a single-step request on `thread_id`.
    /// `step_depth`: 0=INTO, 1=OVER, 2=OUT.
    pub async fn event_request_set_step(
        &self,
        thread_id: u64,
        step_depth: u32,
    ) -> Result<u32> {
        let mut req = Vec::new();
        req.push(EVENT_STEP);
        req.push(2u8); // SUSPEND_ALL
        req.extend_from_slice(&1u32.to_be_bytes()); // 1 modifier
        req.push(10u8); // Step modifier
        write_id(&mut req, thread_id);
        req.extend_from_slice(&STEP_SIZE_LINE.to_be_bytes());
        req.extend_from_slice(&step_depth.to_be_bytes());
        let data = self.send_command(15, 1, req).await?;
        let mut pos = 0;
        read_u32(&data, &mut pos)
    }

    pub async fn event_request_clear(&self, event_kind: u8, request_id: u32) -> Result<()> {
        let mut req = Vec::new();
        req.push(event_kind);
        req.extend_from_slice(&request_id.to_be_bytes());
        self.send_command(15, 2, req).await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // StringReference commands (command set 10)
    // -----------------------------------------------------------------------

    /// Reads the UTF-8 value of a `java.lang.String` object.
    pub async fn string_value(&self, string_id: u64) -> Result<String> {
        let mut req = Vec::new();
        write_id(&mut req, string_id);
        let data = self.send_command(10, 1, req).await?;
        let mut pos = 0;
        read_string(&data, &mut pos)
    }

    /// Returns the `(ref_type_tag, ref_type_id)` of the given object.
    /// Uses ObjectReference.ReferenceType (commandset 9, command 1).
    pub async fn object_reference_type(&self, object_id: u64) -> Result<(u8, u64)> {
        let mut req = Vec::new();
        write_id(&mut req, object_id);
        let data = self.send_command(9, 1, req).await?;
        let mut pos = 0;
        let tag = read_u8(&data, &mut pos)?;
        let type_id = read_id(&data, &mut pos)?;
        Ok((tag, type_id))
    }

    /// Registers an uncaught-exception listener that suspends all threads.
    /// Returns the assigned request ID (needed for `event_request_clear`).
    ///
    /// Uses EventRequest.Set (commandset 15, command 1) with an ExceptionOnly
    /// modifier (kind=8): any class, caught=false, uncaught=true.
    pub async fn event_request_set_exception_uncaught(&self) -> Result<u32> {
        let mut req = Vec::new();
        req.push(EVENT_EXCEPTION);
        req.push(2u8); // SUSPEND_ALL
        req.extend_from_slice(&1u32.to_be_bytes()); // 1 modifier
        req.push(8u8); // ExceptionOnly modifier
        write_id(&mut req, 0u64); // refTypeID = 0 means any class
        req.push(0u8); // caught = false
        req.push(1u8); // uncaught = true
        let data = self.send_command(15, 1, req).await?;
        let mut pos = 0;
        read_u32(&data, &mut pos)
    }
}

// ---------------------------------------------------------------------------
// Wire encoding helpers
// ---------------------------------------------------------------------------

fn read_u8(data: &[u8], pos: &mut usize) -> Result<u8> {
    if *pos >= data.len() {
        return Err(anyhow!("JDWP buffer underrun reading u8 at {pos}"));
    }
    let v = data[*pos];
    *pos += 1;
    Ok(v)
}

fn read_u16(data: &[u8], pos: &mut usize) -> Result<u16> {
    if *pos + 2 > data.len() {
        return Err(anyhow!("JDWP buffer underrun reading u16 at {pos}"));
    }
    let v = u16::from_be_bytes(data[*pos..*pos + 2].try_into().unwrap());
    *pos += 2;
    Ok(v)
}

fn read_u32(data: &[u8], pos: &mut usize) -> Result<u32> {
    if *pos + 4 > data.len() {
        return Err(anyhow!("JDWP buffer underrun reading u32 at {pos}"));
    }
    let v = u32::from_be_bytes(data[*pos..*pos + 4].try_into().unwrap());
    *pos += 4;
    Ok(v)
}

fn read_u64(data: &[u8], pos: &mut usize) -> Result<u64> {
    if *pos + 8 > data.len() {
        return Err(anyhow!("JDWP buffer underrun reading u64 at {pos}"));
    }
    let v = u64::from_be_bytes(data[*pos..*pos + 8].try_into().unwrap());
    *pos += 8;
    Ok(v)
}

/// Reads an ID (assumed 8 bytes on all modern JVMs).
fn read_id(data: &[u8], pos: &mut usize) -> Result<u64> {
    read_u64(data, pos)
}

fn read_string(data: &[u8], pos: &mut usize) -> Result<String> {
    let len = read_u32(data, pos)? as usize;
    if *pos + len > data.len() {
        return Err(anyhow!("JDWP buffer underrun reading string of len {len}"));
    }
    let s = std::str::from_utf8(&data[*pos..*pos + len])
        .map_err(|e| anyhow!("JDWP string not UTF-8: {e}"))?
        .to_string();
    *pos += len;
    Ok(s)
}

fn write_id(buf: &mut Vec<u8>, id: u64) {
    buf.extend_from_slice(&id.to_be_bytes());
}

fn read_tagged_value(data: &[u8], pos: &mut usize) -> Result<JvmValue> {
    let tag = read_u8(data, pos)?;
    match tag {
        b'B' => Ok(JvmValue::Byte(read_u8(data, pos)?)),
        b'C' => Ok(JvmValue::Char(read_u16(data, pos)?)),
        b'D' => {
            let bits = read_u64(data, pos)?;
            Ok(JvmValue::Double(f64::from_bits(bits)))
        }
        b'F' => {
            let bits = read_u32(data, pos)?;
            Ok(JvmValue::Float(f32::from_bits(bits)))
        }
        b'I' => Ok(JvmValue::Int(read_u32(data, pos)?)),
        b'J' => Ok(JvmValue::Long(read_u64(data, pos)?)),
        b'S' => Ok(JvmValue::Short(read_u16(data, pos)?)),
        b'V' => Ok(JvmValue::Void),
        b'Z' => Ok(JvmValue::Boolean(read_u8(data, pos)? != 0)),
        // Object, array, string, thread, class, etc.: all carry an objectID.
        _ => {
            let id = read_id(data, pos)?;
            Ok(JvmValue::Object { tag, id })
        }
    }
}

// ---------------------------------------------------------------------------
// Event parsing
// ---------------------------------------------------------------------------

fn parse_composite_event(data: &[u8]) -> Result<Vec<JdwpEvent>> {
    let mut pos = 0;
    let _suspend_policy = read_u8(data, &mut pos)?;
    let count = read_u32(data, &mut pos)? as usize;
    let mut events = Vec::with_capacity(count);

    for _ in 0..count {
        let kind = read_u8(data, &mut pos)?;
        let request_id = read_u32(data, &mut pos)?;

        match kind {
            EVENT_CLASS_PREPARE => {
                let thread_id = read_id(data, &mut pos)?;
                let ref_type_tag = read_u8(data, &mut pos)?;
                let ref_type_id = read_id(data, &mut pos)?;
                let signature = read_string(data, &mut pos)?;
                let status = read_u32(data, &mut pos)?;
                events.push(JdwpEvent::ClassPrepare(ClassPrepareEvent {
                    request_id,
                    thread_id,
                    ref_type_tag,
                    ref_type_id,
                    signature,
                    status,
                }));
            }
            EVENT_BREAKPOINT | EVENT_STEP => {
                let thread_id = read_id(data, &mut pos)?;
                let type_tag = read_u8(data, &mut pos)?;
                let class_id = read_id(data, &mut pos)?;
                let method_id = read_id(data, &mut pos)?;
                let index = read_u64(data, &mut pos)?;
                let location = Location { type_tag, class_id, method_id, index };
                if kind == EVENT_BREAKPOINT {
                    events.push(JdwpEvent::Breakpoint(BreakpointEvent {
                        request_id,
                        thread_id,
                        location,
                    }));
                } else {
                    events.push(JdwpEvent::Step(StepEvent { request_id, thread_id, location }));
                }
            }
            EVENT_EXCEPTION => {
                let thread_id = read_id(data, &mut pos)?;
                // location where the exception was thrown
                let _type_tag = read_u8(data, &mut pos)?;
                let _class_id = read_id(data, &mut pos)?;
                let _method_id = read_id(data, &mut pos)?;
                let _index = read_u64(data, &mut pos)?;
                // exception: tagged-objectID (tag byte + objectID)
                let _exception_tag = read_u8(data, &mut pos)?;
                let exception_object_id = read_id(data, &mut pos)?;
                // catch_location: location (0-filled when uncaught)
                let _catch_type_tag = read_u8(data, &mut pos)?;
                let _catch_class_id = read_id(data, &mut pos)?;
                let _catch_method_id = read_id(data, &mut pos)?;
                let _catch_index = read_u64(data, &mut pos)?;
                events.push(JdwpEvent::ExceptionUncaught(ExceptionUncaughtEvent {
                    request_id,
                    thread_id,
                    exception_object_id,
                }));
            }
            EVENT_VM_START => {
                // thread_id: objectID — consume it, no action needed
                let _thread_id = read_id(data, &mut pos)?;
            }
            EVENT_VM_DEATH => {
                events.push(JdwpEvent::VmDeath);
            }
            other => {
                // Unknown event kind; we cannot reliably skip it without knowing
                // its payload size, so stop parsing this composite.
                warn!("unknown JDWP event kind {other}, stopping composite parse");
                break;
            }
        }
    }

    Ok(events)
}
