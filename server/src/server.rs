use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use anyhow::Result;
use dap_core::{
    build_tools::get_build_tool,
    jdwp::{
        JdwpClient, JdwpEvent, JvmValue, Location,
        EVENT_BREAKPOINT, EVENT_STEP,
        STEP_DEPTH_INTO, STEP_DEPTH_OVER, STEP_DEPTH_OUT,
    },
    launcher,
    main_class::find_main_classes,
    source_map::{SourceMap, build_source_map},
    types::{Capabilities, DapEvent, DapRequest, DapResponse},
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{Mutex, mpsc::UnboundedSender, oneshot};
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Breakpoint state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum BpState {
    /// Class not yet loaded; install when ClassPrepare fires.
    Pending,
    /// Successfully installed on one or more JVM classes.
    /// Each entry is `(binary_class_name, jdwp_request_id)`.
    Installed { classes: Vec<(String, u32)>, #[allow(dead_code)] installed_line: u32 },
    /// No source mapping or no executable line found at or after the requested line.
    Rejected,
}

#[derive(Debug, Clone)]
struct BreakpointEntry {
    /// DAP-facing breakpoint ID returned to the editor.
    id: u32,
    source_path: PathBuf,
    requested_line: u32,
    state: BpState,
}

// ---------------------------------------------------------------------------
// JVM stopped snapshot
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct SnapshotVariable {
    name: String,
    display_value: String,
}

#[derive(Debug)]
struct SnapshotFrame {
    frame_id: u64,
    /// Last component of the binary class name (e.g. "Main" from "com/example/Main").
    class_name: String,
    method_name: String,
    source_path: Option<PathBuf>,
    line: Option<u32>,
    /// DAP `variablesReference` for the Locals scope.  0 = no variables.
    variables_ref: u32,
    variables: Vec<SnapshotVariable>,
}

#[derive(Debug)]
struct SnapshotThread {
    id: u64,
    name: String,
    frames: Vec<SnapshotFrame>,
}

#[derive(Debug)]
struct JvmSnapshot {
    stopped_thread_id: u64,
    threads: Vec<SnapshotThread>,
}

impl JvmSnapshot {
    /// Finds the frame with the given DAP frame ID across all threads.
    fn find_frame(&self, frame_id: u64) -> Option<&SnapshotFrame> {
        self.threads
            .iter()
            .flat_map(|t| &t.frames)
            .find(|f| f.frame_id == frame_id)
    }

    /// Finds the frame whose Locals scope has the given `variablesReference`.
    fn find_frame_by_vars_ref(&self, vars_ref: u32) -> Option<&SnapshotFrame> {
        self.threads
            .iter()
            .flat_map(|t| &t.frames)
            .find(|f| f.variables_ref == vars_ref)
    }
}

// ---------------------------------------------------------------------------
// Session state
// ---------------------------------------------------------------------------

struct DebugSession {
    jdwp: Arc<JdwpClient>,
    source_map: Arc<SourceMap>,
    /// Dropping this signals the event-loop task to kill the JVM process.
    #[allow(dead_code)]
    kill_tx: oneshot::Sender<()>,
    breakpoints: Vec<BreakpointEntry>,
    next_bp_id: u32,
    /// Populated when the JVM is stopped at a breakpoint or step; cleared on resume.
    snapshot: Option<JvmSnapshot>,
    /// JDWP request ID of the outstanding single-step request, if any.
    pending_step_request: Option<u32>,
    /// Class signature of an observed uncaught exception (e.g.
    /// `"Ljava/lang/RuntimeException;"`).  Set on ExceptionUncaught and
    /// inspected on VmDeath to derive the termination cause.
    pending_exception_class: Option<String>,
}

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

pub struct Backend {
    tx: UnboundedSender<Value>,
    seq: Arc<AtomicU64>,
    session: Arc<Mutex<Option<DebugSession>>>,
}

impl Backend {
    pub fn new(tx: UnboundedSender<Value>) -> Self {
        Self {
            tx,
            seq: Arc::new(AtomicU64::new(0)),
            session: Arc::new(Mutex::new(None)),
        }
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn send(&self, value: Value) {
        if let Err(e) = self.tx.send(value) {
            tracing::error!("channel send error: {e}");
        }
    }

    fn send_response(&self, response: DapResponse) {
        self.send(serde_json::to_value(response).expect("response serialization failed"));
    }

    fn send_event(&self, event: DapEvent) {
        self.send(serde_json::to_value(event).expect("event serialization failed"));
    }

    pub async fn handle_message(&self, msg: Value) {
        let type_ = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if type_ != "request" {
            warn!("unexpected message type: {type_}");
            return;
        }
        let req: DapRequest = match serde_json::from_value(msg) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("failed to parse request: {e}");
                return;
            }
        };
        self.handle_request(req).await;
    }

    async fn handle_request(&self, req: DapRequest) {
        debug!("request: {}", req.command);
        match req.command.as_str() {
            "initialize" => self.handle_initialize(req),
            "launch" => self.handle_launch(req).await,
            "configurationDone" => self.handle_configuration_done(req).await,
            "setBreakpoints" => self.handle_set_breakpoints(req).await,
            "continue" => self.handle_continue(req).await,
            "next" => self.handle_step_command(req, STEP_DEPTH_OVER).await,
            "stepIn" => self.handle_step_command(req, STEP_DEPTH_INTO).await,
            "stepOut" => self.handle_step_command(req, STEP_DEPTH_OUT).await,
            "threads" => self.handle_threads(req).await,
            "stackTrace" => self.handle_stack_trace(req).await,
            "scopes" => self.handle_scopes(req).await,
            "variables" => self.handle_variables(req).await,
            "evaluate" => self.handle_evaluate(req).await,
            "disconnect" => self.handle_disconnect(req).await,
            _ => {
                warn!("unhandled command: {}", req.command);
                self.send_response(DapResponse::err(
                    self.next_seq(),
                    req.seq,
                    &req.command,
                    &format!("unhandled command: {}", req.command),
                ));
            }
        }
    }

    fn handle_initialize(&self, req: DapRequest) {
        let caps = Capabilities {
            supports_configuration_done_request: true,
        };
        self.send_response(DapResponse::ok(
            self.next_seq(),
            req.seq,
            "initialize",
            Some(serde_json::to_value(caps).expect("caps serialization failed")),
        ));
        self.send_event(DapEvent::new(self.next_seq(), "initialized", None));
    }

    async fn handle_launch(&self, req: DapRequest) {
        let args = req.arguments.as_ref().unwrap_or(&Value::Null);

        let project_root = match args.get("projectRoot").and_then(|v| v.as_str()) {
            Some(p) => PathBuf::from(p),
            None => {
                self.send_response(DapResponse::err(
                    self.next_seq(),
                    req.seq,
                    "launch",
                    "launch requires 'projectRoot' argument",
                ));
                self.send_event(DapEvent::new(self.next_seq(), "terminated", None));
                return;
            }
        };

        let explicit_main = args
            .get("mainClass")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let result = self
            .do_launch(project_root, explicit_main, req.seq, &req.command)
            .await;

        if let Err(e) = result {
            let msg = format!("{e:#}");
            tracing::error!("launch failed: {msg}");
            self.send_response(DapResponse::err(
                self.next_seq(),
                req.seq,
                "launch",
                &msg,
            ));
            self.send_event(DapEvent::new(
                self.next_seq(),
                "terminated",
                Some(json!({ "restart": false })),
            ));
        }
    }

    async fn do_launch(
        &self,
        project_root: PathBuf,
        explicit_main: Option<String>,
        req_seq: u64,
        req_cmd: &str,
    ) -> Result<()> {
        let build_tool = get_build_tool(&project_root);

        let root = project_root.clone();
        let tool_clone = build_tool.clone();
        let build_result =
            tokio::task::spawn_blocking(move || tool_clone.build(&root)).await??;

        if !build_result.success {
            let msg = format!("build failed:\n{}", build_result.errors.join("\n"));
            self.send_response(DapResponse::err(self.next_seq(), req_seq, req_cmd, &msg));
            self.send_event(DapEvent::new(self.next_seq(), "terminated", None));
            return Ok(());
        }

        let root = project_root.clone();
        let tool_clone = build_tool.clone();
        let source_roots =
            tokio::task::spawn_blocking(move || tool_clone.get_source_roots(&root)).await??;

        let main_class = if let Some(m) = explicit_main {
            m
        } else {
            let mut candidates = find_main_classes(&source_roots)?;
            match candidates.len() {
                0 => anyhow::bail!(
                    "no main class found under {project_root:?}; \
                     provide 'mainClass' in launch configuration"
                ),
                1 => candidates.remove(0).fully_qualified_name,
                _ => {
                    let names: Vec<_> = candidates
                        .iter()
                        .map(|c| c.fully_qualified_name.as_str())
                        .collect();
                    warn!(
                        "multiple main classes found: {names:?}; \
                         launching first. Provide 'mainClass' to be explicit."
                    );
                    candidates.remove(0).fully_qualified_name
                }
            }
        };

        let root = project_root.clone();
        let tool_clone = build_tool.clone();
        let classpath =
            tokio::task::spawn_blocking(move || tool_clone.get_classpath(&root)).await??;

        let classpath_dirs: Vec<PathBuf> =
            classpath.iter().filter(|p| p.is_dir()).cloned().collect();
        let source_roots_clone = source_roots.clone();
        let source_map =
            tokio::task::spawn_blocking(move || {
                build_source_map(&classpath_dirs, &source_roots_clone)
            })
            .await??;

        let (jdwp, mut process, event_rx) =
            launcher::launch(&project_root, &classpath, &main_class).await?;

        // Take the child process out of the JvmProcess wrapper so the event-loop
        // task can own it (for kill-on-disconnect and exit-code retrieval).
        let mut child = process.take_child();
        let stderr = child.stderr.take().expect("stderr is piped");

        let (kill_tx, kill_rx) = oneshot::channel::<()>();

        // Register an uncaught-exception listener so we can derive the
        // termination cause (clean_exit vs. uncaught_exception vs. out_of_memory)
        // when VmDeath fires.
        if let Err(e) = jdwp.event_request_set_exception_uncaught().await {
            warn!("failed to register uncaught-exception listener: {e}");
        }

        *self.session.lock().await = Some(DebugSession {
            jdwp: jdwp.clone(),
            source_map: Arc::new(source_map),
            kill_tx,
            breakpoints: Vec::new(),
            next_bp_id: 1,
            snapshot: None,
            pending_step_request: None,
            pending_exception_class: None,
        });

        self.spawn_stderr_task(stderr);
        self.spawn_event_loop(event_rx, child, kill_rx);

        self.send_response(DapResponse::ok(self.next_seq(), req_seq, req_cmd, None));
        Ok(())
    }

    async fn handle_set_breakpoints(&self, req: DapRequest) {
        let args = req.arguments.as_ref().unwrap_or(&Value::Null);

        let source_path = match args
            .get("source")
            .and_then(|s| s.get("path"))
            .and_then(|p| p.as_str())
        {
            Some(p) => PathBuf::from(p),
            None => {
                self.send_response(DapResponse::err(
                    self.next_seq(),
                    req.seq,
                    "setBreakpoints",
                    "setBreakpoints requires 'source.path'",
                ));
                return;
            }
        };

        let requested_lines: Vec<u32> = args
            .get("breakpoints")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|bp| bp.get("line").and_then(|l| l.as_u64()).map(|l| l as u32))
                    .collect()
            })
            .unwrap_or_default();

        let session_data = {
            let mut guard = self.session.lock().await;
            match guard.as_mut() {
                None => None,
                Some(session) => {
                    let old_jdwp_ids: Vec<u32> = session
                        .breakpoints
                        .iter()
                        .filter(|e| e.source_path == source_path)
                        .flat_map(|e| {
                            if let BpState::Installed { classes, .. } = &e.state {
                                classes.iter().map(|(_, id)| *id).collect::<Vec<_>>()
                            } else {
                                vec![]
                            }
                        })
                        .collect();

                    session.breakpoints.retain(|e| e.source_path != source_path);

                    let mut new_entries: Vec<(u32, u32)> = Vec::new();
                    for &line in &requested_lines {
                        let id = session.next_bp_id;
                        session.next_bp_id += 1;
                        new_entries.push((id, line));
                        session.breakpoints.push(BreakpointEntry {
                            id,
                            source_path: source_path.clone(),
                            requested_line: line,
                            state: BpState::Pending,
                        });
                    }

                    let class_names: Vec<String> =
                        session.source_map.classes_for_source(&source_path).to_vec();

                    Some((session.jdwp.clone(), class_names, old_jdwp_ids, new_entries))
                }
            }
        };

        let (jdwp, class_names, old_jdwp_ids, new_entries) = match session_data {
            None => {
                let bps: Vec<Value> = requested_lines
                    .iter()
                    .map(|&line| json!({ "verified": false, "line": line }))
                    .collect();
                self.send_response(DapResponse::ok(
                    self.next_seq(),
                    req.seq,
                    "setBreakpoints",
                    Some(json!({ "breakpoints": bps })),
                ));
                return;
            }
            Some(data) => data,
        };

        for jdwp_id in old_jdwp_ids {
            if let Err(e) = jdwp.event_request_clear(EVENT_BREAKPOINT, jdwp_id).await {
                warn!("failed to clear old breakpoint {jdwp_id}: {e}");
            }
        }

        // (bp_id, Option<(installed_line, Vec<(class_name, jdwp_id)>)>)
        let bp_results: Vec<(u32, Option<(u32, Vec<(String, u32)>)>)> = if class_names.is_empty() {
            new_entries.iter().map(|(id, _)| (*id, None)).collect()
        } else {
            let mut results = Vec::with_capacity(new_entries.len());
            for &(id, requested_line) in &new_entries {
                let installed =
                    install_on_loaded_classes(&jdwp, &class_names, requested_line).await;
                results.push((id, installed));
            }
            results
        };

        let final_states: Vec<(u32, BpState)> = bp_results
            .iter()
            .map(|(id, result)| {
                let state = match result {
                    None if class_names.is_empty() => BpState::Rejected,
                    None => BpState::Pending,
                    Some((installed_line, classes)) => BpState::Installed {
                        classes: classes.clone(),
                        installed_line: *installed_line,
                    },
                };
                (*id, state)
            })
            .collect();

        {
            let mut guard = self.session.lock().await;
            if let Some(session) = guard.as_mut() {
                for (id, state) in &final_states {
                    if let Some(entry) = session.breakpoints.iter_mut().find(|e| e.id == *id) {
                        entry.state = state.clone();
                    }
                }
            }
        }

        let bps_json: Vec<Value> = new_entries
            .iter()
            .zip(bp_results.iter())
            .map(|((id, requested_line), (_, result))| match result {
                None => json!({ "id": id, "verified": false, "line": requested_line }),
                Some((installed_line, _)) => {
                    json!({ "id": id, "verified": true, "line": installed_line })
                }
            })
            .collect();

        self.send_response(DapResponse::ok(
            self.next_seq(),
            req.seq,
            "setBreakpoints",
            Some(json!({ "breakpoints": bps_json })),
        ));
    }

    async fn handle_continue(&self, req: DapRequest) {
        // Respond immediately; the JVM resume happens below.
        self.send_response(DapResponse::ok(
            self.next_seq(),
            req.seq,
            "continue",
            Some(json!({ "allThreadsContinued": true })),
        ));

        let (jdwp, old_step_req) = {
            let mut guard = self.session.lock().await;
            match guard.as_mut() {
                None => return,
                Some(s) => {
                    s.snapshot = None;
                    (s.jdwp.clone(), s.pending_step_request.take())
                }
            }
        };

        // Clear any outstanding step request so it does not fire after resume.
        if let Some(req_id) = old_step_req {
            if let Err(e) = jdwp.event_request_clear(EVENT_STEP, req_id).await {
                warn!("failed to clear step request on continue: {e}");
            }
        }

        if let Err(e) = jdwp.vm_resume().await {
            warn!("vm_resume on continue failed: {e}");
        }
    }

    async fn handle_step_command(&self, req: DapRequest, step_depth: u32) {
        let (jdwp, thread_id, old_step_req) = {
            let mut guard = self.session.lock().await;
            match guard.as_mut() {
                None => {
                    self.send_response(DapResponse::err(
                        self.next_seq(),
                        req.seq,
                        &req.command,
                        "no active debug session",
                    ));
                    return;
                }
                Some(s) => {
                    let thread_id = s
                        .snapshot
                        .as_ref()
                        .map(|snap| snap.stopped_thread_id)
                        .unwrap_or(0);
                    if thread_id == 0 {
                        self.send_response(DapResponse::err(
                            self.next_seq(),
                            req.seq,
                            &req.command,
                            "no stopped thread",
                        ));
                        return;
                    }
                    s.snapshot = None;
                    (s.jdwp.clone(), thread_id, s.pending_step_request.take())
                }
            }
        };

        // Clear any previous step request before registering a new one.
        if let Some(old_id) = old_step_req {
            if let Err(e) = jdwp.event_request_clear(EVENT_STEP, old_id).await {
                warn!("failed to clear old step request: {e}");
            }
        }

        let step_req_id = match jdwp.event_request_set_step(thread_id, step_depth).await {
            Ok(id) => id,
            Err(e) => {
                warn!("event_request_set_step failed: {e}");
                self.send_response(DapResponse::err(
                    self.next_seq(),
                    req.seq,
                    &req.command,
                    &format!("step request failed: {e}"),
                ));
                return;
            }
        };

        {
            let mut guard = self.session.lock().await;
            if let Some(s) = guard.as_mut() {
                s.pending_step_request = Some(step_req_id);
            }
        }

        self.send_response(DapResponse::ok(self.next_seq(), req.seq, &req.command, None));

        if let Err(e) = jdwp.vm_resume().await {
            warn!("vm_resume after step request failed: {e}");
        }
    }

    async fn handle_threads(&self, req: DapRequest) {
        let threads_json = {
            let guard = self.session.lock().await;
            match guard.as_ref().and_then(|s| s.snapshot.as_ref()) {
                None => vec![],
                Some(snap) => snap
                    .threads
                    .iter()
                    .map(|t| json!({ "id": t.id, "name": t.name }))
                    .collect::<Vec<_>>(),
            }
        };

        self.send_response(DapResponse::ok(
            self.next_seq(),
            req.seq,
            "threads",
            Some(json!({ "threads": threads_json })),
        ));
    }

    async fn handle_stack_trace(&self, req: DapRequest) {
        let thread_id = req
            .arguments
            .as_ref()
            .and_then(|a| a.get("threadId"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        let frames_json = {
            let guard = self.session.lock().await;
            match guard.as_ref().and_then(|s| s.snapshot.as_ref()) {
                None => vec![],
                Some(snap) => {
                    let thread = snap.threads.iter().find(|t| t.id == thread_id);
                    match thread {
                        None => vec![],
                        Some(t) => t
                            .frames
                            .iter()
                            .map(|f| {
                                let source = f.source_path.as_ref().map(|p| {
                                    json!({ "path": p.to_string_lossy() })
                                });
                                json!({
                                    "id": f.frame_id,
                                    "name": format!("{}.{}", f.class_name, f.method_name),
                                    "source": source,
                                    "line": f.line.unwrap_or(0),
                                    "column": 0,
                                })
                            })
                            .collect::<Vec<_>>(),
                    }
                }
            }
        };

        self.send_response(DapResponse::ok(
            self.next_seq(),
            req.seq,
            "stackTrace",
            Some(json!({
                "stackFrames": frames_json,
                "totalFrames": frames_json.len(),
            })),
        ));
    }

    async fn handle_scopes(&self, req: DapRequest) {
        let frame_id = req
            .arguments
            .as_ref()
            .and_then(|a| a.get("frameId"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        let scopes_json = {
            let guard = self.session.lock().await;
            match guard.as_ref().and_then(|s| s.snapshot.as_ref()) {
                None => vec![],
                Some(snap) => match snap.find_frame(frame_id) {
                    None => vec![],
                    Some(frame) => {
                        vec![json!({
                            "name": "Locals",
                            "variablesReference": frame.variables_ref,
                            "expensive": false,
                        })]
                    }
                },
            }
        };

        self.send_response(DapResponse::ok(
            self.next_seq(),
            req.seq,
            "scopes",
            Some(json!({ "scopes": scopes_json })),
        ));
    }

    async fn handle_variables(&self, req: DapRequest) {
        let vars_ref = req
            .arguments
            .as_ref()
            .and_then(|a| a.get("variablesReference"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32;

        let variables_json = {
            let guard = self.session.lock().await;
            match guard.as_ref().and_then(|s| s.snapshot.as_ref()) {
                None => vec![],
                Some(snap) => match snap.find_frame_by_vars_ref(vars_ref) {
                    None => vec![],
                    Some(frame) => frame
                        .variables
                        .iter()
                        .map(|v| {
                            json!({
                                "name": v.name,
                                "value": v.display_value,
                                "variablesReference": 0,
                            })
                        })
                        .collect::<Vec<_>>(),
                },
            }
        };

        self.send_response(DapResponse::ok(
            self.next_seq(),
            req.seq,
            "variables",
            Some(json!({ "variables": variables_json })),
        ));
    }

    async fn handle_evaluate(&self, req: DapRequest) {
        let args = req.arguments.as_ref().unwrap_or(&Value::Null);

        let expression = match args.get("expression").and_then(|v| v.as_str()) {
            Some(e) => e.to_string(),
            None => {
                self.send_response(DapResponse::err(
                    self.next_seq(), req.seq, "evaluate", "missing expression",
                ));
                return;
            }
        };

        let frame_id = args.get("frameId").and_then(|v| v.as_u64());

        let result = {
            let guard = self.session.lock().await;
            guard.as_ref().and_then(|s| s.snapshot.as_ref()).and_then(|snap| {
                let frame = if let Some(fid) = frame_id {
                    snap.find_frame(fid)
                } else {
                    // No frameId: use the top frame of the stopped thread.
                    snap.threads
                        .iter()
                        .find(|t| t.id == snap.stopped_thread_id)
                        .and_then(|t| t.frames.first())
                };
                frame.and_then(|f| {
                    f.variables
                        .iter()
                        .find(|v| v.name == expression)
                        .map(|v| v.display_value.clone())
                })
            })
        };

        match result {
            Some(value) => self.send_response(DapResponse::ok(
                self.next_seq(),
                req.seq,
                "evaluate",
                Some(json!({ "result": value, "variablesReference": 0 })),
            )),
            None => self.send_response(DapResponse::err(
                self.next_seq(),
                req.seq,
                "evaluate",
                "not available",
            )),
        }
    }

    /// Spawns a task that forwards JVM stderr lines as DAP `output` events.
    fn spawn_stderr_task(&self, stderr: tokio::process::ChildStderr) {
        let tx = self.tx.clone();
        let seq = self.seq.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let s = seq.fetch_add(1, Ordering::Relaxed) + 1;
                let ev = DapEvent::new(
                    s,
                    "output",
                    Some(json!({ "category": "stderr", "output": format!("{line}\n") })),
                );
                let _ = tx.send(serde_json::to_value(ev).unwrap());
            }
        });
    }

    /// Spawns a task that consumes JDWP events and sends the appropriate DAP
    /// events.  Handles ClassPrepare (deferred breakpoint install), Breakpoint
    /// (snapshot + stopped event), VmDeath, and kill-on-disconnect signaling.
    ///
    /// When the JVM exits (VmDeath or JDWP channel close), the task waits
    /// briefly for the OS process to finish, collects the exit code, and sends
    /// a `terminated` event (with `exitCode` when non-zero).
    ///
    /// When the session is dropped (disconnect), `kill_rx` resolves, which
    /// causes the task to kill the JVM process and send `terminated`.
    fn spawn_event_loop(
        &self,
        mut event_rx: tokio::sync::mpsc::UnboundedReceiver<JdwpEvent>,
        mut child: tokio::process::Child,
        mut kill_rx: oneshot::Receiver<()>,
    ) {
        let tx = self.tx.clone();
        let seq = self.seq.clone();
        let session = self.session.clone();

        tokio::spawn(async move {
            let kill_active = true;

            let exit_code: Option<i32> = 'event_loop: loop {
                tokio::select! {
                    ev = event_rx.recv() => match ev {
                        None | Some(JdwpEvent::VmDeath) => {
                            // Wait up to 1 s for the OS process to exit so we
                            // can collect the exit code.
                            let code = tokio::time::timeout(
                                std::time::Duration::from_secs(1),
                                child.wait(),
                            )
                            .await
                            .ok()
                            .and_then(|r| r.ok())
                            .and_then(|s| s.code());
                            break 'event_loop code;
                        }

                        Some(JdwpEvent::ExceptionUncaught(ev)) => {
                            // Query the exception class while the JVM is still
                            // suspended, so we can derive the termination cause.
                            let class_sig: Option<String> = async {
                                let (jdwp, already_set) = {
                                    let guard = session.lock().await;
                                    let s = guard.as_ref()?;
                                    if s.pending_exception_class.is_some() {
                                        return None; // keep first exception
                                    }
                                    (s.jdwp.clone(), false)
                                };
                                let _ = already_set;
                                let (_, type_id) =
                                    jdwp.object_reference_type(ev.exception_object_id).await.ok()?;
                                let sig = jdwp.ref_type_name(type_id).await.ok()?;
                                Some(sig)
                            }
                            .await;

                            {
                                let mut guard = session.lock().await;
                                if let Some(s) = guard.as_mut() {
                                    if s.pending_exception_class.is_none() {
                                        s.pending_exception_class = class_sig;
                                    }
                                    if let Err(e) = s.jdwp.vm_resume().await {
                                        warn!("vm_resume after ExceptionUncaught failed: {e}");
                                    }
                                }
                            }
                        }

                        Some(JdwpEvent::ClassPrepare(ev)) => {
                            let binary_name = signature_to_binary_name(&ev.signature);

                            // Collect pending bps (DeferredBreakpointInstall) and
                            // already-installed bps for sibling classes (ExtendBreakpointInstall).
                            let (jdwp, pending_bps, extend_bps) = {
                                let guard = session.lock().await;
                                match guard.as_ref() {
                                    None => continue,
                                    Some(s) => {
                                        let source = s
                                            .source_map
                                            .source_for_class(&binary_name)
                                            .map(|p| p.to_path_buf());
                                        let (pending, extend) = match &source {
                                            None => (vec![], vec![]),
                                            Some(src) => {
                                                let pending: Vec<(u32, u32)> = s
                                                    .breakpoints
                                                    .iter()
                                                    .filter(|e| {
                                                        e.source_path == *src
                                                            && matches!(e.state, BpState::Pending)
                                                    })
                                                    .map(|e| (e.id, e.requested_line))
                                                    .collect();
                                                let extend: Vec<(u32, u32)> = s
                                                    .breakpoints
                                                    .iter()
                                                    .filter(|e| {
                                                        e.source_path == *src
                                                            && if let BpState::Installed {
                                                                classes, ..
                                                            } = &e.state
                                                            {
                                                                !classes
                                                                    .iter()
                                                                    .any(|(cn, _)| cn == &binary_name)
                                                            } else {
                                                                false
                                                            }
                                                    })
                                                    .map(|e| (e.id, e.requested_line))
                                                    .collect();
                                                (pending, extend)
                                            }
                                        };
                                        (s.jdwp.clone(), pending, extend)
                                    }
                                }
                            };

                            if pending_bps.is_empty() && extend_bps.is_empty() {
                                let _ = jdwp.vm_resume().await;
                                continue;
                            }

                            let all_lines = build_all_lines_for_class(
                                &jdwp,
                                ev.ref_type_tag,
                                ev.ref_type_id,
                            )
                            .await;

                            // DeferredBreakpointInstall: install pending bps on this class.
                            // Limit the allowed gap to 2 lines so that a class whose bytecode
                            // does not cover the requested line (e.g. the outer class when the bp
                            // targets a line inside an anonymous inner class) does not steal the
                            // install. If this class has no bytecode within 2 lines, the bp stays
                            // Pending until a sibling class that does cover the line loads.
                            const MAX_GAP: u32 = 2;
                            let mut deferred_installed: Vec<(u32, u32, u32)> = Vec::new();
                            for (bp_id, requested_line) in &pending_bps {
                                if let Some((installed_line, jdwp_id)) =
                                    try_install_at_nearest_line(
                                        &jdwp, &all_lines, *requested_line, MAX_GAP,
                                    )
                                    .await
                                {
                                    deferred_installed.push((*bp_id, installed_line, jdwp_id));
                                }
                                // If not installed, bp stays Pending — a later sibling class may cover it.
                            }

                            // ExtendBreakpointInstall: add this class to already-verified bps.
                            let mut extend_installed: Vec<(u32, u32)> = Vec::new();
                            for (bp_id, requested_line) in &extend_bps {
                                if let Some((_line, jdwp_id)) =
                                    try_install_at_nearest_line(
                                        &jdwp, &all_lines, *requested_line, MAX_GAP,
                                    )
                                    .await
                                {
                                    extend_installed.push((*bp_id, jdwp_id));
                                }
                            }

                            {
                                let mut guard = session.lock().await;
                                if let Some(s) = guard.as_mut() {
                                    for (bp_id, installed_line, jdwp_id) in &deferred_installed {
                                        if let Some(entry) =
                                            s.breakpoints.iter_mut().find(|e| e.id == *bp_id)
                                        {
                                            entry.state = BpState::Installed {
                                                classes: vec![(binary_name.clone(), *jdwp_id)],
                                                installed_line: *installed_line,
                                            };
                                        }
                                    }
                                    for (bp_id, jdwp_id) in &extend_installed {
                                        if let Some(entry) =
                                            s.breakpoints.iter_mut().find(|e| e.id == *bp_id)
                                        {
                                            if let BpState::Installed { classes, .. } =
                                                &mut entry.state
                                            {
                                                classes.push((binary_name.clone(), *jdwp_id));
                                            }
                                        }
                                    }
                                }
                            }

                            // Notify DAP only for newly-verified (deferred) breakpoints.
                            for (bp_id, installed_line, _) in &deferred_installed {
                                let s = seq.fetch_add(1, Ordering::Relaxed) + 1;
                                let ev = DapEvent::new(
                                    s,
                                    "breakpoint",
                                    Some(json!({
                                        "reason": "changed",
                                        "breakpoint": { "id": bp_id, "verified": true, "line": installed_line }
                                    })),
                                );
                                let _ = tx.send(serde_json::to_value(ev).unwrap());
                            }

                            let _ = jdwp.vm_resume().await;
                        }

                        Some(JdwpEvent::Breakpoint(ev)) => {
                            let (jdwp, source_map) = {
                                let guard = session.lock().await;
                                match guard.as_ref() {
                                    None => continue,
                                    Some(s) => (s.jdwp.clone(), s.source_map.clone()),
                                }
                            };

                            let snapshot =
                                build_snapshot(&jdwp, &source_map, ev.thread_id).await;

                            {
                                let mut guard = session.lock().await;
                                if let Some(s) = guard.as_mut() {
                                    s.snapshot = Some(snapshot);
                                }
                            }

                            let s = seq.fetch_add(1, Ordering::Relaxed) + 1;
                            let stopped = DapEvent::new(
                                s,
                                "stopped",
                                Some(json!({
                                    "reason": "breakpoint",
                                    "threadId": ev.thread_id,
                                    "allThreadsStopped": true,
                                })),
                            );
                            let _ = tx.send(serde_json::to_value(stopped).unwrap());
                        }

                        Some(JdwpEvent::Step(ev)) => {
                            // Clear the step request and build a snapshot.
                            let (jdwp, source_map, step_req_id) = {
                                let mut guard = session.lock().await;
                                match guard.as_mut() {
                                    None => continue,
                                    Some(s) => (
                                        s.jdwp.clone(),
                                        s.source_map.clone(),
                                        s.pending_step_request.take(),
                                    ),
                                }
                            };

                            if let Some(req_id) = step_req_id {
                                if let Err(e) =
                                    jdwp.event_request_clear(EVENT_STEP, req_id).await
                                {
                                    warn!("failed to clear step request after firing: {e}");
                                }
                            }

                            let snapshot =
                                build_snapshot(&jdwp, &source_map, ev.thread_id).await;

                            {
                                let mut guard = session.lock().await;
                                if let Some(s) = guard.as_mut() {
                                    s.snapshot = Some(snapshot);
                                }
                            }

                            let s = seq.fetch_add(1, Ordering::Relaxed) + 1;
                            let stopped = DapEvent::new(
                                s,
                                "stopped",
                                Some(json!({
                                    "reason": "step",
                                    "threadId": ev.thread_id,
                                    "allThreadsStopped": true,
                                })),
                            );
                            let _ = tx.send(serde_json::to_value(stopped).unwrap());
                        }
                    },
                    _ = &mut kill_rx, if kill_active => {
                        let _ = child.kill().await;
                        break 'event_loop None;
                    }
                }
            };

            let exception_class = {
                let mut guard = session.lock().await;
                let ec = guard
                    .as_mut()
                    .and_then(|s| s.pending_exception_class.take());
                *guard = None;
                ec
            };

            let termination_cause = derive_termination_cause(exception_class.as_deref());

            let s = seq.fetch_add(1, Ordering::Relaxed) + 1;
            let mut body_map = serde_json::Map::new();
            if let Some(code) = exit_code.filter(|&c| c != 0) {
                body_map.insert("exitCode".to_string(), json!(code));
            }
            body_map.insert("terminationCause".to_string(), json!(termination_cause));
            let event = DapEvent::new(s, "terminated", Some(Value::Object(body_map)));
            let _ = tx.send(serde_json::to_value(event).unwrap());
        });
    }

    async fn handle_configuration_done(&self, req: DapRequest) {
        self.send_response(DapResponse::ok(
            self.next_seq(),
            req.seq,
            "configurationDone",
            None,
        ));

        if let Some(session) = self.session.lock().await.as_ref() {
            if let Err(e) = session.jdwp.vm_resume().await {
                warn!("vm_resume failed: {e}");
            }
        }
    }

    async fn handle_disconnect(&self, req: DapRequest) {
        self.send_response(DapResponse::ok(
            self.next_seq(),
            req.seq,
            "disconnect",
            None,
        ));

        let had_session = {
            let mut guard = self.session.lock().await;
            let had = guard.is_some();
            // Dropping kill_tx signals the event-loop task to kill the JVM.
            // The event-loop task will then send the terminated event.
            *guard = None;
            had
        };

        // When no session was active the event loop is not running, so send
        // terminated directly.
        if !had_session {
            self.send_event(DapEvent::new(self.next_seq(), "terminated", None));
        }
    }
}

// ---------------------------------------------------------------------------
// Snapshot builder
// ---------------------------------------------------------------------------

/// Builds a full `JvmSnapshot` from the current JVM state.  The JVM must be
/// fully suspended (SuspendPolicy.ALL was set on the event) before calling.
async fn build_snapshot(
    jdwp: &JdwpClient,
    source_map: &SourceMap,
    stopped_thread_id: u64,
) -> JvmSnapshot {
    let thread_ids = jdwp.vm_all_threads().await.unwrap_or_default();

    let mut threads: Vec<SnapshotThread> = Vec::new();
    let mut next_vars_ref: u32 = 1;

    // Cache: class_id → (binary_name, methods: Vec<(method_id, method_name)>)
    let mut class_cache: HashMap<u64, (String, Vec<(u64, String)>)> = HashMap::new();

    for thread_id in thread_ids {
        let name = jdwp
            .thread_name(thread_id)
            .await
            .unwrap_or_else(|_| format!("Thread-{thread_id:#x}"));

        let raw_frames = jdwp
            .thread_frames(thread_id, 0, -1i32 as u32)
            .await
            .unwrap_or_default();

        let mut frames: Vec<SnapshotFrame> = Vec::new();

        for raw in raw_frames {
            let loc = &raw.location;

            // Populate class/method cache if this class has not been seen.
            if !class_cache.contains_key(&loc.class_id) {
                let sig = jdwp
                    .ref_type_name(loc.class_id)
                    .await
                    .unwrap_or_default();
                let binary = signature_to_binary_name(&sig);
                let methods = jdwp
                    .ref_type_methods(loc.class_id)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|m| (m.method_id, m.name))
                    .collect::<Vec<_>>();
                class_cache.insert(loc.class_id, (binary, methods));
            }

            let (binary_name, methods) = class_cache.get(&loc.class_id).unwrap();
            let class_name = binary_name
                .rsplit('/')
                .next()
                .unwrap_or(binary_name.as_str())
                .to_string();
            let method_name = methods
                .iter()
                .find(|(mid, _)| *mid == loc.method_id)
                .map(|(_, name)| name.clone())
                .unwrap_or_else(|| format!("method-{:#x}", loc.method_id));

            let source_path = source_map
                .source_for_class(binary_name)
                .map(|p| p.to_path_buf());

            // Resolve source line from the line table.
            let line = match jdwp.method_line_table(loc.class_id, loc.method_id).await {
                Ok(table) => table
                    .lines
                    .iter()
                    .filter(|e| e.line_code_index <= loc.index)
                    .max_by_key(|e| e.line_code_index)
                    .map(|e| e.line_number),
                Err(_) => None,
            };

            // Collect in-scope local variables.
            let variables =
                collect_frame_variables(jdwp, thread_id, raw.frame_id, loc.class_id, loc.method_id, loc.index)
                    .await;

            let variables_ref = if variables.is_empty() { 0 } else { next_vars_ref };
            if variables_ref != 0 {
                next_vars_ref += 1;
            }

            frames.push(SnapshotFrame {
                frame_id: raw.frame_id,
                class_name,
                method_name,
                source_path,
                line,
                variables_ref,
                variables,
            });
        }

        threads.push(SnapshotThread { id: thread_id, name, frames });
    }

    JvmSnapshot { stopped_thread_id, threads }
}

/// Collects the local variables currently in scope for a stack frame.
async fn collect_frame_variables(
    jdwp: &JdwpClient,
    thread_id: u64,
    frame_id: u64,
    class_id: u64,
    method_id: u64,
    bytecode_index: u64,
) -> Vec<SnapshotVariable> {
    let var_table = match jdwp.method_variable_table(class_id, method_id).await {
        Ok(t) => t,
        Err(_) => return vec![], // no debug info
    };

    let in_scope: Vec<_> = var_table
        .iter()
        .filter(|v| {
            v.code_index <= bytecode_index
                && bytecode_index < v.code_index + v.length as u64
        })
        .collect();

    if in_scope.is_empty() {
        return vec![];
    }

    let slots: Vec<(u32, u8)> = in_scope
        .iter()
        .map(|v| (v.slot, tag_for_signature(&v.signature)))
        .collect();

    let values = match jdwp.stack_frame_get_values(thread_id, frame_id, &slots).await {
        Ok(v) => v,
        Err(e) => {
            debug!("stack_frame_get_values failed: {e}");
            return vec![];
        }
    };

    let mut result = Vec::with_capacity(in_scope.len());
    for (var, value) in in_scope.iter().zip(values.iter()) {
        let display_value = format_value(jdwp, value).await;
        result.push(SnapshotVariable {
            name: var.name.clone(),
            display_value,
        });
    }
    result
}

/// Formats a `JvmValue` as a human-readable display string.
/// Strings are fetched and quoted; null objects are shown as `null`.
async fn format_value(jdwp: &JdwpClient, value: &JvmValue) -> String {
    if let JvmValue::Object { tag, id } = value {
        if *id == 0 {
            return "null".to_string();
        }
        if *tag == b's' {
            return match jdwp.string_value(*id).await {
                Ok(s) => format!("\"{s}\""),
                Err(_) => format!("@{id:#x}"),
            };
        }
    }
    value.display()
}

/// Returns the JDWP type tag for a variable descriptor.
/// Primitives use their own tag letter; object and array references use `L`.
fn tag_for_signature(sig: &str) -> u8 {
    match sig.bytes().next() {
        Some(b'L') | Some(b'[') | None => b'L',
        Some(b) => b,
    }
}

// ---------------------------------------------------------------------------
// Breakpoint helpers
// ---------------------------------------------------------------------------

fn signature_to_binary_name(signature: &str) -> String {
    let s = signature.strip_prefix('L').unwrap_or(signature);
    s.strip_suffix(';').unwrap_or(s).to_string()
}

async fn build_all_lines_for_class(
    jdwp: &JdwpClient,
    ref_type_tag: u8,
    ref_type_id: u64,
) -> Vec<(u32, Location)> {
    let methods = match jdwp.ref_type_methods(ref_type_id).await {
        Ok(m) => m,
        Err(e) => {
            warn!("ref_type_methods failed for class {ref_type_id}: {e}");
            return vec![];
        }
    };
    let mut lines: Vec<(u32, Location)> = Vec::new();
    for method in methods {
        match jdwp.method_line_table(ref_type_id, method.method_id).await {
            Ok(table) => {
                for entry in table.lines {
                    lines.push((
                        entry.line_number,
                        Location {
                            type_tag: ref_type_tag,
                            class_id: ref_type_id,
                            method_id: method.method_id,
                            index: entry.line_code_index,
                        },
                    ));
                }
            }
            Err(e) => {
                debug!("method_line_table skipped for method {}: {e}", method.method_id);
            }
        }
    }
    lines.sort_by_key(|(ln, _)| *ln);
    lines
}

/// Installs a breakpoint at `requested_line` on every already-loaded JVM class
/// whose binary name appears in `class_names`.  Returns the installed line
/// (minimum adjusted line across all classes) and one `(class_name, jdwp_id)`
/// pair per successful install.  Returns `None` when no class is currently
/// loaded or none of them has an executable line at or after `requested_line`.
async fn install_on_loaded_classes(
    jdwp: &JdwpClient,
    class_names: &[String],
    requested_line: u32,
) -> Option<(u32, Vec<(String, u32)>)> {
    let loaded = match jdwp.vm_all_classes().await {
        Ok(c) => c,
        Err(e) => {
            warn!("vm_all_classes failed: {e}");
            return None;
        }
    };

    let mut installed_line: Option<u32> = None;
    let mut results: Vec<(String, u32)> = Vec::new();

    for cls in &loaded {
        let bin = signature_to_binary_name(&cls.signature);
        if !class_names.contains(&bin) {
            continue;
        }
        let lines = build_all_lines_for_class(jdwp, cls.ref_type_tag, cls.ref_type_id).await;
        if let Some((line, jdwp_id)) =
            try_install_at_nearest_line(jdwp, &lines, requested_line, 2).await
        {
            if installed_line.map_or(true, |prev| line < prev) {
                installed_line = Some(line);
            }
            results.push((bin, jdwp_id));
        }
    }

    installed_line.map(|line| (line, results))
}

/// Finds the nearest executable line at or after `requested_line` (up to
/// `max_gap` lines away) and installs a JDWP breakpoint there.
/// Returns `None` when no line is found within the gap or the install fails.
async fn try_install_at_nearest_line(
    jdwp: &JdwpClient,
    all_lines: &[(u32, Location)],
    requested_line: u32,
    max_gap: u32,
) -> Option<(u32, u32)> {
    let (actual_line, location) = all_lines
        .iter()
        .find(|(ln, _)| *ln >= requested_line && *ln <= requested_line + max_gap)?;
    match jdwp.event_request_set_breakpoint(location).await {
        Ok(jdwp_id) => Some((*actual_line, jdwp_id)),
        Err(e) => {
            warn!("event_request_set_breakpoint at line {actual_line} failed: {e}");
            None
        }
    }
}

/// Derives the termination cause from the observed uncaught exception class
/// signature (if any).
///
/// - `None` → `"clean_exit"` (no uncaught exception was observed)
/// - `Some(sig)` where sig contains `"OutOfMemoryError"` → `"out_of_memory"`
/// - `Some(_)` any other exception → `"uncaught_exception"`
fn derive_termination_cause(exception_class: Option<&str>) -> &'static str {
    match exception_class {
        None => "clean_exit",
        Some(sig) if sig.contains("OutOfMemoryError") => "out_of_memory",
        Some(_) => "uncaught_exception",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(feature = "integration-test")]
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::sync::mpsc::unbounded_channel;
    use std::time::Duration;

    async fn run_exchange(requests: Vec<Value>) -> Vec<Value> {
        let (tx, mut rx) = unbounded_channel::<Value>();
        let backend = Backend::new(tx);
        for req in requests {
            backend.handle_message(req).await;
        }
        drop(backend);
        let mut responses = Vec::new();
        while let Some(msg) = rx.recv().await {
            responses.push(msg);
        }
        responses
    }

    /// Drain `rx` until no message arrives within `timeout`, then return all collected.
    async fn collect_with_timeout(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<Value>,
        timeout: Duration,
    ) -> Vec<Value> {
        let mut msgs = Vec::new();
        loop {
            match tokio::time::timeout(timeout, rx.recv()).await {
                Ok(Some(msg)) => msgs.push(msg),
                _ => break,
            }
        }
        msgs
    }

    /// Drain `rx`, collecting each message, until `predicate` returns true
    /// for one of them or `timeout` elapses.
    async fn collect_until<F>(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<Value>,
        predicate: F,
        timeout: Duration,
    ) -> Vec<Value>
    where
        F: Fn(&Value) -> bool,
    {
        let mut msgs = Vec::new();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or(Duration::ZERO);
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(msg)) => {
                    let done = predicate(&msg);
                    msgs.push(msg);
                    if done {
                        break;
                    }
                }
                _ => break,
            }
        }
        msgs
    }

    #[tokio::test]
    async fn test_initialize_response_and_initialized_event() {
        let msgs = run_exchange(vec![json!({
            "seq": 1, "type": "request", "command": "initialize",
            "arguments": { "adapterID": "dapintar" }
        })])
        .await;

        assert_eq!(msgs.len(), 2, "expected initialize response + initialized event");
        assert_eq!(msgs[0]["command"], "initialize");
        assert_eq!(msgs[0]["success"], true);
        assert_eq!(msgs[0]["body"]["supportsConfigurationDoneRequest"], true);
        assert_eq!(msgs[1]["event"], "initialized");
    }

    #[tokio::test]
    async fn test_initialize_then_configure_handshake() {
        let msgs = run_exchange(vec![
            json!({"seq": 1, "type": "request", "command": "initialize",
                   "arguments": {"adapterID": "dapintar"}}),
            json!({"seq": 2, "type": "request", "command": "configurationDone"}),
        ])
        .await;

        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["command"], "initialize");
        assert_eq!(msgs[1]["event"], "initialized");
        assert_eq!(msgs[2]["command"], "configurationDone");
        assert_eq!(msgs[2]["success"], true);
    }

    #[tokio::test]
    async fn test_launch_missing_project_root_returns_error() {
        let msgs = run_exchange(vec![json!({
            "seq": 1, "type": "request", "command": "launch",
            "arguments": { "mainClass": "Main" }
        })])
        .await;

        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["command"], "launch");
        assert_eq!(msgs[0]["success"], false);
        assert_eq!(msgs[1]["event"], "terminated");
    }

    #[tokio::test]
    async fn test_unknown_command_returns_error_response() {
        let msgs = run_exchange(vec![json!({
            "seq": 1, "type": "request", "command": "unknownCommand"
        })])
        .await;

        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["success"], false);
        assert_eq!(msgs[0]["command"], "unknownCommand");
    }

    #[tokio::test]
    async fn test_disconnect_sends_terminated_event() {
        let msgs = run_exchange(vec![json!({
            "seq": 1, "type": "request", "command": "disconnect"
        })])
        .await;

        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["command"], "disconnect");
        assert_eq!(msgs[0]["success"], true);
        assert_eq!(msgs[1]["event"], "terminated");
    }

    // -----------------------------------------------------------------------
    // JVM launch helpers
    // -----------------------------------------------------------------------

    fn simple_java_fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap()
            .join("dap_core").join("tests").join("fixtures").join("simple_java")
    }

    fn breakpoints_java_fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap()
            .join("dap_core").join("tests").join("fixtures").join("breakpoints_java")
    }

    fn breakable_source() -> PathBuf {
        breakpoints_java_fixture()
            .join("src").join("main").join("java").join("Breakable.java")
    }

    // -----------------------------------------------------------------------
    // Launch / session tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_launch_simple_java_jdwp_connected() {
        let (tx, mut rx) = unbounded_channel::<Value>();
        let backend = Arc::new(Backend::new(tx));

        backend.handle_message(json!({"seq": 1, "type": "request", "command": "initialize",
            "arguments": {"adapterID": "dapintar"}})).await;
        backend.handle_message(json!({"seq": 2, "type": "request", "command": "launch",
            "arguments": {"projectRoot": simple_java_fixture(), "mainClass": "Main"}})).await;
        backend.handle_message(json!({"seq": 3, "type": "request", "command": "configurationDone"})).await;

        let msgs = collect_with_timeout(&mut rx, Duration::from_secs(15)).await;

        assert!(
            msgs.iter().any(|m| m["command"] == "launch" && m["success"] == true),
            "expected successful launch; msgs: {msgs:?}"
        );
        assert!(
            msgs.iter().any(|m| m["event"] == "terminated"),
            "expected terminated; msgs: {msgs:?}"
        );
    }

    #[tokio::test]
    async fn test_launch_bad_main_class_sends_terminated() {
        let (tx, mut rx) = unbounded_channel::<Value>();
        let backend = Arc::new(Backend::new(tx));

        backend.handle_message(json!({"seq": 1, "type": "request", "command": "launch",
            "arguments": {"projectRoot": simple_java_fixture(), "mainClass": "NonExistentMain"}})).await;
        backend.handle_message(json!({"seq": 2, "type": "request", "command": "configurationDone"})).await;

        let msgs = collect_with_timeout(&mut rx, Duration::from_secs(15)).await;

        assert!(msgs.iter().any(|m| m["event"] == "terminated"), "msgs: {msgs:?}");
    }

    // -----------------------------------------------------------------------
    // Breakpoint management tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_set_breakpoint_deferred_then_verified() {
        let (tx, mut rx) = unbounded_channel::<Value>();
        let backend = Arc::new(Backend::new(tx));

        backend.handle_message(json!({"seq": 1, "type": "request", "command": "initialize",
            "arguments": {"adapterID": "dapintar"}})).await;
        backend.handle_message(json!({"seq": 2, "type": "request", "command": "launch",
            "arguments": {"projectRoot": breakpoints_java_fixture(), "mainClass": "Breakable"}})).await;
        backend.handle_message(json!({"seq": 3, "type": "request", "command": "setBreakpoints",
            "arguments": {
                "source": {"path": breakable_source()},
                "breakpoints": [{"line": 4}]
            }})).await;
        backend.handle_message(json!({"seq": 4, "type": "request", "command": "configurationDone"})).await;

        let msgs = collect_with_timeout(&mut rx, Duration::from_secs(15)).await;

        let set_bp = msgs.iter().find(|m| m["command"] == "setBreakpoints")
            .expect("no setBreakpoints response");
        assert_eq!(set_bp["success"], true);
        assert_eq!(set_bp["body"]["breakpoints"][0]["verified"], false,
            "should be unverified before class loads; msgs: {msgs:?}");

        let bp_ev = msgs.iter().find(|m| m["event"] == "breakpoint")
            .expect("no breakpoint event; msgs: {msgs:?}");
        assert_eq!(bp_ev["body"]["breakpoint"]["verified"], true,
            "should be verified after ClassPrepare; msgs: {msgs:?}");
    }

    #[tokio::test]
    async fn test_set_breakpoint_line_adjusted_to_nearest_executable() {
        let (tx, mut rx) = unbounded_channel::<Value>();
        let backend = Arc::new(Backend::new(tx));

        backend.handle_message(json!({"seq": 1, "type": "request", "command": "initialize",
            "arguments": {"adapterID": "dapintar"}})).await;
        backend.handle_message(json!({"seq": 2, "type": "request", "command": "launch",
            "arguments": {"projectRoot": breakpoints_java_fixture(), "mainClass": "Breakable"}})).await;
        // Line 2 is blank — never executable.
        backend.handle_message(json!({"seq": 3, "type": "request", "command": "setBreakpoints",
            "arguments": {
                "source": {"path": breakable_source()},
                "breakpoints": [{"line": 2}]
            }})).await;
        backend.handle_message(json!({"seq": 4, "type": "request", "command": "configurationDone"})).await;

        let msgs = collect_with_timeout(&mut rx, Duration::from_secs(15)).await;

        let bp_ev = msgs.iter().find(|m| m["event"] == "breakpoint")
            .expect("no breakpoint event; msgs: {msgs:?}");
        let bp = &bp_ev["body"]["breakpoint"];
        assert_eq!(bp["verified"], true);
        let adjusted = bp["line"].as_u64().expect("no line in breakpoint event");
        assert!(adjusted >= 3,
            "expected blank line 2 adjusted to first executable line (>= 3); got {adjusted}");
    }

    #[tokio::test]
    async fn test_set_breakpoint_no_source_mapping_stays_unverified() {
        let (tx, mut rx) = unbounded_channel::<Value>();
        let backend = Arc::new(Backend::new(tx));

        backend.handle_message(json!({"seq": 1, "type": "request", "command": "initialize",
            "arguments": {"adapterID": "dapintar"}})).await;
        backend.handle_message(json!({"seq": 2, "type": "request", "command": "launch",
            "arguments": {"projectRoot": simple_java_fixture(), "mainClass": "Main"}})).await;
        backend.handle_message(json!({"seq": 3, "type": "request", "command": "setBreakpoints",
            "arguments": {
                "source": {"path": "/nonexistent/NeverLoads.java"},
                "breakpoints": [{"line": 5}]
            }})).await;
        backend.handle_message(json!({"seq": 4, "type": "request", "command": "configurationDone"})).await;

        let msgs = collect_with_timeout(&mut rx, Duration::from_secs(15)).await;

        let set_bp = msgs.iter().find(|m| m["command"] == "setBreakpoints")
            .expect("no setBreakpoints response");
        assert_eq!(set_bp["body"]["breakpoints"][0]["verified"], false);

        let has_verified = msgs.iter().any(|m| {
            m["event"] == "breakpoint" && m["body"]["breakpoint"]["verified"] == true
        });
        assert!(!has_verified, "unexpected verified breakpoint event; msgs: {msgs:?}");
        assert!(msgs.iter().any(|m| m["event"] == "terminated"), "msgs: {msgs:?}");
    }

    // -----------------------------------------------------------------------
    // Breakpoint hit and inspection tests
    // -----------------------------------------------------------------------

    /// Sets up the backend through to a breakpoint hit, returning (backend, rx,
    /// stopped_thread_id).  The JVM is suspended at the hit.
    async fn setup_breakpoint_hit() -> (
        Arc<Backend>,
        tokio::sync::mpsc::UnboundedReceiver<Value>,
        u64,
    ) {
        let (tx, mut rx) = unbounded_channel::<Value>();
        let backend = Arc::new(Backend::new(tx));

        backend.handle_message(json!({"seq": 1, "type": "request", "command": "initialize",
            "arguments": {"adapterID": "dapintar"}})).await;
        backend.handle_message(json!({"seq": 2, "type": "request", "command": "launch",
            "arguments": {"projectRoot": breakpoints_java_fixture(), "mainClass": "Breakable"}})).await;
        // Line 8 is `System.out.println(...)` — all local variables are in scope.
        backend.handle_message(json!({"seq": 3, "type": "request", "command": "setBreakpoints",
            "arguments": {
                "source": {"path": breakable_source()},
                "breakpoints": [{"line": 8}]
            }})).await;
        backend.handle_message(json!({"seq": 4, "type": "request", "command": "configurationDone"})).await;

        // Wait until the stopped event arrives (JVM hit the breakpoint).
        let msgs = collect_until(
            &mut rx,
            |m| m["event"] == "stopped",
            Duration::from_secs(15),
        )
        .await;

        let stopped_thread_id = msgs
            .iter()
            .find(|m| m["event"] == "stopped")
            .expect("no stopped event; did the breakpoint install fail?")["body"]["threadId"]
            .as_u64()
            .expect("no threadId in stopped event");

        (backend, rx, stopped_thread_id)
    }

    /// Launch program and hit breakpoint — assert `stopped` event received with
    /// reason="breakpoint".
    #[tokio::test]
    async fn test_breakpoint_hit_stopped_event() {
        let (backend, rx, stopped_thread_id) = setup_breakpoint_hit().await;
        drop(backend); // keep it alive until here so the JVM doesn't die early

        // We already verified a stopped event arrived in setup_breakpoint_hit.
        assert!(stopped_thread_id > 0, "thread id should be non-zero");
        let _ = rx; // consumed in setup
    }

    /// After breakpoint stop, request `threads` — assert stopped thread appears.
    #[tokio::test]
    async fn test_threads_after_breakpoint_hit() {
        let (backend, mut rx, stopped_thread_id) = setup_breakpoint_hit().await;

        backend.handle_message(json!({"seq": 5, "type": "request", "command": "threads"})).await;
        let msgs = collect_with_timeout(&mut rx, Duration::from_secs(5)).await;

        let resp = msgs.iter().find(|m| m["command"] == "threads")
            .expect("no threads response; msgs: {msgs:?}");
        assert_eq!(resp["success"], true);

        let threads = resp["body"]["threads"].as_array().expect("no threads array");
        assert!(
            threads.iter().any(|t| t["id"].as_u64() == Some(stopped_thread_id)),
            "stopped thread {stopped_thread_id} not found in threads list; threads: {threads:?}"
        );

        drop(backend);
    }

    /// After breakpoint stop, request `stackTrace` — assert frames include the
    /// main method of Breakable.
    #[tokio::test]
    async fn test_stack_trace_after_breakpoint_hit() {
        let (backend, mut rx, stopped_thread_id) = setup_breakpoint_hit().await;

        backend.handle_message(json!({"seq": 5, "type": "request", "command": "stackTrace",
            "arguments": {"threadId": stopped_thread_id}})).await;
        let msgs = collect_with_timeout(&mut rx, Duration::from_secs(5)).await;

        let resp = msgs.iter().find(|m| m["command"] == "stackTrace")
            .expect("no stackTrace response; msgs: {msgs:?}");
        assert_eq!(resp["success"], true);

        let frames = resp["body"]["stackFrames"].as_array().expect("no stackFrames");
        assert!(!frames.is_empty(), "expected at least one frame");

        let top = &frames[0];
        let frame_name = top["name"].as_str().unwrap_or("");
        assert!(
            frame_name.contains("main"),
            "top frame should contain 'main'; got '{frame_name}'"
        );

        drop(backend);
    }

    /// After breakpoint stop, request `variables` — assert local variables with
    /// correct names and display values.
    #[tokio::test]
    async fn test_variables_after_breakpoint_hit() {
        let (backend, mut rx, stopped_thread_id) = setup_breakpoint_hit().await;

        // stackTrace to get the top frame ID.
        backend.handle_message(json!({"seq": 5, "type": "request", "command": "stackTrace",
            "arguments": {"threadId": stopped_thread_id}})).await;
        let st_msgs = collect_with_timeout(&mut rx, Duration::from_secs(5)).await;
        let st_resp = st_msgs.iter().find(|m| m["command"] == "stackTrace").unwrap();
        let top_frame_id = st_resp["body"]["stackFrames"][0]["id"].as_u64().unwrap();

        // scopes for the top frame.
        backend.handle_message(json!({"seq": 6, "type": "request", "command": "scopes",
            "arguments": {"frameId": top_frame_id}})).await;
        let sc_msgs = collect_with_timeout(&mut rx, Duration::from_secs(5)).await;
        let sc_resp = sc_msgs.iter().find(|m| m["command"] == "scopes").unwrap();
        let vars_ref = sc_resp["body"]["scopes"][0]["variablesReference"].as_u64().unwrap();
        assert!(vars_ref > 0, "expected non-zero variablesReference");

        // variables for the Locals scope.
        backend.handle_message(json!({"seq": 7, "type": "request", "command": "variables",
            "arguments": {"variablesReference": vars_ref}})).await;
        let v_msgs = collect_with_timeout(&mut rx, Duration::from_secs(5)).await;
        let v_resp = v_msgs.iter().find(|m| m["command"] == "variables").unwrap();
        let variables = v_resp["body"]["variables"].as_array()
            .expect("no variables array");

        let find = |name: &str| variables.iter().find(|v| v["name"] == name);

        assert_eq!(find("a").expect("variable 'a' missing")["value"], "10");
        assert_eq!(find("b").expect("variable 'b' missing")["value"], "20");
        assert_eq!(find("c").expect("variable 'c' missing")["value"], "30");
        assert_eq!(find("msg").expect("variable 'msg' missing")["value"], "\"hello\"");

        drop(backend);
    }

    // -----------------------------------------------------------------------
    // Execution control tests
    // -----------------------------------------------------------------------

    fn stepable_java_fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap()
            .join("dap_core").join("tests").join("fixtures").join("stepable_java")
    }

    fn stepable_source() -> PathBuf {
        stepable_java_fixture()
            .join("src").join("main").join("java").join("Stepable.java")
    }

    /// Launches Stepable, sets a breakpoint at the given line, sends
    /// configurationDone, and waits for the `stopped` event.  Returns
    /// (backend, rx, stopped_thread_id).
    async fn setup_step_hit(
        bp_line: u64,
    ) -> (Arc<Backend>, tokio::sync::mpsc::UnboundedReceiver<Value>, u64) {
        let (tx, mut rx) = unbounded_channel::<Value>();
        let backend = Arc::new(Backend::new(tx));

        backend.handle_message(json!({"seq": 1, "type": "request", "command": "initialize",
            "arguments": {"adapterID": "dapintar"}})).await;
        backend.handle_message(json!({"seq": 2, "type": "request", "command": "launch",
            "arguments": {"projectRoot": stepable_java_fixture(), "mainClass": "Stepable"}})).await;
        backend.handle_message(json!({"seq": 3, "type": "request", "command": "setBreakpoints",
            "arguments": {
                "source": {"path": stepable_source()},
                "breakpoints": [{"line": bp_line}]
            }})).await;
        backend.handle_message(json!({"seq": 4, "type": "request", "command": "configurationDone"})).await;

        let msgs = collect_until(&mut rx, |m| m["event"] == "stopped", Duration::from_secs(15)).await;
        let stopped_thread_id = msgs
            .iter()
            .find(|m| m["event"] == "stopped")
            .expect("no stopped event in setup_step_hit")["body"]["threadId"]
            .as_u64()
            .expect("no threadId");

        (backend, rx, stopped_thread_id)
    }

    /// Helper: request stackTrace and return the name of the top frame.
    async fn top_frame_name(
        backend: &Backend,
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<Value>,
        thread_id: u64,
        seq: u64,
    ) -> String {
        backend.handle_message(json!({"seq": seq, "type": "request", "command": "stackTrace",
            "arguments": {"threadId": thread_id}})).await;
        let msgs = collect_with_timeout(rx, Duration::from_secs(5)).await;
        msgs.iter()
            .find(|m| m["command"] == "stackTrace")
            .and_then(|m| m["body"]["stackFrames"][0]["name"].as_str())
            .unwrap_or("")
            .to_string()
    }

    /// Hit breakpoint, send `continue` — program resumes and terminates.
    #[tokio::test]
    async fn test_continue_resumes_jvm() {
        // Line 8: `int a = 10;` in Stepable.main
        let (backend, mut rx, _) = setup_step_hit(8).await;

        backend.handle_message(json!({"seq": 5, "type": "request", "command": "continue"})).await;

        let msgs = collect_with_timeout(&mut rx, Duration::from_secs(15)).await;
        assert!(
            msgs.iter().any(|m| m["event"] == "terminated"),
            "expected terminated after continue; msgs: {msgs:?}"
        );

        drop(backend);
    }

    /// Hit breakpoint at line 8, send `next` — stopped event at next line in main.
    #[tokio::test]
    async fn test_next_advances_to_next_line() {
        // Line 8: `int a = 10;` in Stepable.main
        let (backend, mut rx, stopped_thread_id) = setup_step_hit(8).await;

        backend.handle_message(json!({"seq": 5, "type": "request", "command": "next",
            "arguments": {"threadId": stopped_thread_id}})).await;

        let step_msgs = collect_until(&mut rx, |m| m["event"] == "stopped", Duration::from_secs(15)).await;
        let step_stopped = step_msgs.iter().find(|m| m["event"] == "stopped")
            .expect("no stopped event after next; msgs: {step_msgs:?}");
        assert_eq!(step_stopped["body"]["reason"], "step",
            "stopped reason should be 'step'");

        // The top frame should still be in main (not stepped into add).
        let name = top_frame_name(&backend, &mut rx, stopped_thread_id, 6).await;
        assert!(name.contains("main"), "after next, should still be in main; got '{name}'");

        drop(backend);
    }

    /// Hit breakpoint at the `add(a, b)` call site (line 10), send `stepIn`
    /// — stopped inside the `add` method.
    #[tokio::test]
    async fn test_step_in_enters_called_method() {
        // Line 10: `int c = add(a, b);` — the call site
        let (backend, mut rx, stopped_thread_id) = setup_step_hit(10).await;

        backend.handle_message(json!({"seq": 5, "type": "request", "command": "stepIn",
            "arguments": {"threadId": stopped_thread_id}})).await;

        let step_msgs = collect_until(&mut rx, |m| m["event"] == "stopped", Duration::from_secs(15)).await;
        step_msgs.iter().find(|m| m["event"] == "stopped")
            .expect("no stopped event after stepIn; msgs: {step_msgs:?}");

        let name = top_frame_name(&backend, &mut rx, stopped_thread_id, 6).await;
        assert!(
            name.contains("add"),
            "after stepIn, should be inside 'add'; got '{name}'"
        );

        drop(backend);
    }

    /// Stop inside `add` (line 4), send `stepOut` — stopped back in main.
    #[tokio::test]
    async fn test_step_out_returns_to_caller() {
        // Line 4: `return x + y;` inside add
        let (backend, mut rx, stopped_thread_id) = setup_step_hit(4).await;

        backend.handle_message(json!({"seq": 5, "type": "request", "command": "stepOut",
            "arguments": {"threadId": stopped_thread_id}})).await;

        let step_msgs = collect_until(&mut rx, |m| m["event"] == "stopped", Duration::from_secs(15)).await;
        step_msgs.iter().find(|m| m["event"] == "stopped")
            .expect("no stopped event after stepOut; msgs: {step_msgs:?}");

        let name = top_frame_name(&backend, &mut rx, stopped_thread_id, 6).await;
        assert!(
            name.contains("main"),
            "after stepOut, should be back in main; got '{name}'"
        );

        drop(backend);
    }

    // -----------------------------------------------------------------------
    // Evaluate / hover tests
    // -----------------------------------------------------------------------

    /// Hover over a local variable while paused — assert the correct display
    /// value is returned.
    #[tokio::test]
    async fn test_evaluate_hover_local_variable() {
        let (backend, mut rx, stopped_thread_id) = setup_breakpoint_hit().await;

        // Get the top frame ID so we can pass it to evaluate.
        backend.handle_message(json!({"seq": 5, "type": "request", "command": "stackTrace",
            "arguments": {"threadId": stopped_thread_id}})).await;
        let st_msgs = collect_with_timeout(&mut rx, Duration::from_secs(5)).await;
        let top_frame_id = st_msgs
            .iter()
            .find(|m| m["command"] == "stackTrace")
            .unwrap()["body"]["stackFrames"][0]["id"]
            .as_u64()
            .unwrap();

        backend.handle_message(json!({"seq": 6, "type": "request", "command": "evaluate",
            "arguments": {
                "expression": "a",
                "frameId": top_frame_id,
                "context": "hover"
            }})).await;

        let eval_msgs = collect_with_timeout(&mut rx, Duration::from_secs(5)).await;
        let resp = eval_msgs.iter().find(|m| m["command"] == "evaluate")
            .expect("no evaluate response; msgs: {eval_msgs:?}");
        assert_eq!(resp["success"], true);
        assert_eq!(resp["body"]["result"], "10");

        drop(backend);
    }

    /// Hover over a name that is not in scope — assert an error response.
    #[tokio::test]
    async fn test_evaluate_hover_unknown_name_returns_error() {
        let (backend, mut rx, stopped_thread_id) = setup_breakpoint_hit().await;

        backend.handle_message(json!({"seq": 5, "type": "request", "command": "stackTrace",
            "arguments": {"threadId": stopped_thread_id}})).await;
        let st_msgs = collect_with_timeout(&mut rx, Duration::from_secs(5)).await;
        let top_frame_id = st_msgs
            .iter()
            .find(|m| m["command"] == "stackTrace")
            .unwrap()["body"]["stackFrames"][0]["id"]
            .as_u64()
            .unwrap();

        backend.handle_message(json!({"seq": 6, "type": "request", "command": "evaluate",
            "arguments": {
                "expression": "noSuchVariable",
                "frameId": top_frame_id,
                "context": "hover"
            }})).await;

        let eval_msgs = collect_with_timeout(&mut rx, Duration::from_secs(5)).await;
        let resp = eval_msgs.iter().find(|m| m["command"] == "evaluate")
            .expect("no evaluate response; msgs: {eval_msgs:?}");
        assert_eq!(resp["success"], false);

        drop(backend);
    }

    // -----------------------------------------------------------------------
    // Session termination tests
    // -----------------------------------------------------------------------

    fn exception_java_fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap()
            .join("dap_core").join("tests").join("fixtures").join("exception_java")
    }

    fn oom_java_fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap()
            .join("dap_core").join("tests").join("fixtures").join("oom_java")
    }

    /// Clean exit: terminationCause must be "clean_exit".
    #[tokio::test]
    async fn test_terminated_cause_clean_exit() {
        let (tx, mut rx) = unbounded_channel::<Value>();
        let backend = Arc::new(Backend::new(tx));

        backend.handle_message(json!({"seq": 1, "type": "request", "command": "launch",
            "arguments": {"projectRoot": simple_java_fixture(), "mainClass": "Main"}})).await;
        backend.handle_message(json!({"seq": 2, "type": "request", "command": "configurationDone"})).await;

        let msgs = collect_until(&mut rx, |m| m["event"] == "terminated", Duration::from_secs(15)).await;

        let term = msgs.iter().find(|m| m["event"] == "terminated")
            .expect("no terminated event; msgs: {msgs:?}");
        assert_eq!(term["body"]["terminationCause"], "clean_exit",
            "expected clean_exit; msgs: {msgs:?}");

        drop(backend);
    }

    /// Uncaught exception: terminationCause must be "uncaught_exception".
    #[tokio::test]
    async fn test_terminated_cause_uncaught_exception() {
        let (tx, mut rx) = unbounded_channel::<Value>();
        let backend = Arc::new(Backend::new(tx));

        backend.handle_message(json!({"seq": 1, "type": "request", "command": "launch",
            "arguments": {"projectRoot": exception_java_fixture(), "mainClass": "Exceptional"}})).await;
        backend.handle_message(json!({"seq": 2, "type": "request", "command": "configurationDone"})).await;

        let msgs = collect_until(&mut rx, |m| m["event"] == "terminated", Duration::from_secs(15)).await;

        let term = msgs.iter().find(|m| m["event"] == "terminated")
            .expect("no terminated event; msgs: {msgs:?}");
        assert_eq!(term["body"]["terminationCause"], "uncaught_exception",
            "expected uncaught_exception; msgs: {msgs:?}");

        drop(backend);
    }

    /// OutOfMemoryError: terminationCause must be "out_of_memory".
    #[tokio::test]
    async fn test_terminated_cause_oom() {
        let (tx, mut rx) = unbounded_channel::<Value>();
        let backend = Arc::new(Backend::new(tx));

        backend.handle_message(json!({"seq": 1, "type": "request", "command": "launch",
            "arguments": {"projectRoot": oom_java_fixture(), "mainClass": "OomClass"}})).await;
        backend.handle_message(json!({"seq": 2, "type": "request", "command": "configurationDone"})).await;

        let msgs = collect_until(&mut rx, |m| m["event"] == "terminated", Duration::from_secs(15)).await;

        let term = msgs.iter().find(|m| m["event"] == "terminated")
            .expect("no terminated event; msgs: {msgs:?}");
        assert_eq!(term["body"]["terminationCause"], "out_of_memory",
            "expected out_of_memory; msgs: {msgs:?}");

        drop(backend);
    }

    /// Program that throws an uncaught exception: terminated event should carry
    /// a non-zero exit code and stderr output events should contain the message.
    #[tokio::test]
    async fn test_jvm_exception_sends_terminated_with_exit_code_and_stderr() {
        let (tx, mut rx) = unbounded_channel::<Value>();
        let backend = Arc::new(Backend::new(tx));

        backend.handle_message(json!({"seq": 1, "type": "request", "command": "launch",
            "arguments": {"projectRoot": exception_java_fixture(), "mainClass": "Exceptional"}})).await;
        backend.handle_message(json!({"seq": 2, "type": "request", "command": "configurationDone"})).await;

        let msgs = collect_until(
            &mut rx,
            |m| m["event"] == "terminated",
            Duration::from_secs(15),
        ).await;

        let terminated = msgs.iter().find(|m| m["event"] == "terminated")
            .expect("no terminated event; msgs: {msgs:?}");
        let exit_code = terminated["body"]["exitCode"].as_u64().unwrap_or(0);
        assert!(exit_code != 0, "expected non-zero exit code; msgs: {msgs:?}");

        assert!(
            msgs.iter().any(|m| {
                m["event"] == "output"
                    && m["body"]["category"] == "stderr"
                    && m["body"]["output"]
                        .as_str()
                        .map(|s| s.contains("intentional-exception"))
                        .unwrap_or(false)
            }),
            "expected 'intentional-exception' in stderr output; msgs: {msgs:?}"
        );

        drop(backend);
    }

    /// Disconnect while the JVM is suspended (before configurationDone) — the
    /// process should be killed and a terminated event should arrive.
    #[tokio::test]
    async fn test_disconnect_kills_jvm_and_sends_terminated() {
        let (tx, mut rx) = unbounded_channel::<Value>();
        let backend = Arc::new(Backend::new(tx));

        // Launch but do not resume so the JVM stays suspended.
        backend.handle_message(json!({"seq": 1, "type": "request", "command": "launch",
            "arguments": {"projectRoot": simple_java_fixture(), "mainClass": "Main"}})).await;

        // Wait until launch response arrives before disconnecting.
        let _ = collect_until(
            &mut rx,
            |m| m["command"] == "launch",
            Duration::from_secs(15),
        ).await;

        backend.handle_message(json!({"seq": 2, "type": "request", "command": "disconnect"})).await;

        let msgs = collect_until(
            &mut rx,
            |m| m["event"] == "terminated",
            Duration::from_secs(10),
        ).await;

        assert!(msgs.iter().any(|m| m["event"] == "terminated"),
            "expected terminated after disconnect; msgs: {msgs:?}");

        drop(backend);
    }

    // -----------------------------------------------------------------------
    // Multi-module tests
    // -----------------------------------------------------------------------

    fn multi_module_java_fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap()
            .join("dap_core").join("tests").join("fixtures").join("multi_module_java")
    }

    fn lib_source() -> PathBuf {
        multi_module_java_fixture()
            .join("lib").join("src").join("main").join("java").join("Lib.java")
    }

    /// Hit a breakpoint in the lib subproject class while running App from
    /// the multi-module build.
    #[tokio::test]
    async fn test_breakpoint_in_subproject_class() {
        let (tx, mut rx) = unbounded_channel::<Value>();
        let backend = Arc::new(Backend::new(tx));

        backend.handle_message(json!({"seq": 1, "type": "request", "command": "initialize",
            "arguments": {"adapterID": "dapintar"}})).await;
        backend.handle_message(json!({"seq": 2, "type": "request", "command": "launch",
            "arguments": {"projectRoot": multi_module_java_fixture(), "mainClass": "App"}})).await;
        // Lib.java line 3: `int result = 42;`
        backend.handle_message(json!({"seq": 3, "type": "request", "command": "setBreakpoints",
            "arguments": {
                "source": {"path": lib_source()},
                "breakpoints": [{"line": 3}]
            }})).await;
        backend.handle_message(json!({"seq": 4, "type": "request", "command": "configurationDone"})).await;

        let msgs = collect_until(
            &mut rx,
            |m| m["event"] == "stopped",
            Duration::from_secs(30),
        ).await;

        let stopped = msgs.iter().find(|m| m["event"] == "stopped")
            .expect("expected stopped event; msgs: {msgs:?}");
        assert_eq!(stopped["body"]["reason"], "breakpoint");

        let stopped_tid = stopped["body"]["threadId"].as_u64().unwrap();
        backend.handle_message(json!({"seq": 5, "type": "request", "command": "stackTrace",
            "arguments": {"threadId": stopped_tid}})).await;

        let st_msgs = collect_with_timeout(&mut rx, Duration::from_secs(5)).await;
        let frames = &st_msgs
            .iter()
            .find(|m| m["command"] == "stackTrace")
            .expect("no stackTrace response")["body"]["stackFrames"];
        let top = &frames[0];
        assert!(
            top["name"].as_str().map(|n| n.contains("Lib")).unwrap_or(false),
            "expected top frame to be in Lib; got {top:?}"
        );

        drop(backend);
    }

    // -----------------------------------------------------------------------
    // Inner / anonymous class breakpoint tests
    // -----------------------------------------------------------------------

    fn inner_class_java_fixture() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent().unwrap()
            .join("dap_core").join("tests").join("fixtures").join("inner_class_java")
    }

    fn outer_class_source() -> PathBuf {
        inner_class_java_fixture()
            .join("src").join("main").join("java").join("OuterClass.java")
    }

    /// Set a breakpoint at line 7 — inside the anonymous Runnable's run() method,
    /// which lives in OuterClass$1 (not OuterClass).  The breakpoint must fire
    /// at that exact line even though OuterClass$1 is loaded lazily.
    #[tokio::test]
    async fn test_breakpoint_in_inner_class_fires_at_correct_line() {
        let (tx, mut rx) = unbounded_channel::<Value>();
        let backend = Arc::new(Backend::new(tx));

        backend.handle_message(json!({"seq": 1, "type": "request", "command": "initialize",
            "arguments": {"adapterID": "dapintar"}})).await;
        backend.handle_message(json!({"seq": 2, "type": "request", "command": "launch",
            "arguments": {"projectRoot": inner_class_java_fixture(), "mainClass": "OuterClass"}})).await;
        // Line 7: `System.out.println("anonymous");` inside OuterClass$1.run()
        backend.handle_message(json!({"seq": 3, "type": "request", "command": "setBreakpoints",
            "arguments": {
                "source": {"path": outer_class_source()},
                "breakpoints": [{"line": 7}]
            }})).await;
        backend.handle_message(json!({"seq": 4, "type": "request", "command": "configurationDone"})).await;

        let msgs = collect_until(
            &mut rx,
            |m| m["event"] == "stopped",
            Duration::from_secs(20),
        ).await;

        let stopped = msgs.iter().find(|m| m["event"] == "stopped")
            .expect("expected stopped event at inner-class line; msgs: {msgs:?}");
        assert_eq!(stopped["body"]["reason"], "breakpoint");

        let tid = stopped["body"]["threadId"].as_u64().unwrap();
        backend.handle_message(json!({"seq": 5, "type": "request", "command": "stackTrace",
            "arguments": {"threadId": tid}})).await;

        let st_msgs = collect_with_timeout(&mut rx, Duration::from_secs(5)).await;
        let frames = &st_msgs
            .iter()
            .find(|m| m["command"] == "stackTrace")
            .expect("no stackTrace response")["body"]["stackFrames"];
        let top = &frames[0];

        // The top frame should be OuterClass$1.run, not OuterClass.main
        let frame_name = top["name"].as_str().unwrap_or("");
        assert!(
            frame_name.contains("run"),
            "expected top frame to be in run() of anonymous class; got '{frame_name}'"
        );
        let frame_line = top["line"].as_u64().unwrap_or(0);
        assert_eq!(frame_line, 7, "expected stopped at line 7; got {frame_line}");

        drop(backend);
    }
}
