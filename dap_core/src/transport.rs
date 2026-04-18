use anyhow::{anyhow, Result};
use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

pub struct DapReader<R> {
    inner: BufReader<R>,
}

pub struct DapWriter<W> {
    inner: W,
}

impl<R: tokio::io::AsyncRead + Unpin> DapReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            inner: BufReader::new(reader),
        }
    }

    pub async fn read_message(&mut self) -> Result<Value> {
        let mut content_length: Option<usize> = None;

        loop {
            let mut line = String::new();
            let n = self.inner.read_line(&mut line).await?;
            if n == 0 {
                return Err(anyhow!("connection closed"));
            }
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some(val) = trimmed.strip_prefix("Content-Length: ") {
                content_length = Some(val.trim().parse()?);
            }
        }

        let length = content_length.ok_or_else(|| anyhow!("missing Content-Length header"))?;
        let mut body = vec![0u8; length];
        self.inner.read_exact(&mut body).await?;

        Ok(serde_json::from_slice(&body)?)
    }
}

impl<W: tokio::io::AsyncWrite + Unpin> DapWriter<W> {
    pub fn new(writer: W) -> Self {
        Self { inner: writer }
    }

    pub async fn send<T: Serialize>(&mut self, msg: &T) -> Result<()> {
        let body = serde_json::to_string(msg)?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        self.inner.write_all(header.as_bytes()).await?;
        self.inner.write_all(body.as_bytes()).await?;
        self.inner.flush().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::duplex;

    #[tokio::test]
    async fn test_framing_round_trip() {
        let (client_half, server_half) = duplex(4096);
        let (_client_read, client_write) = tokio::io::split(client_half);
        let (server_read, _server_write) = tokio::io::split(server_half);

        let mut writer = DapWriter::new(client_write);
        let mut reader = DapReader::new(server_read);

        let msg = json!({
            "seq": 1,
            "type": "request",
            "command": "initialize",
            "arguments": { "adapterID": "dapintar" }
        });

        writer.send(&msg).await.unwrap();
        let received = reader.read_message().await.unwrap();

        assert_eq!(received, msg);
    }

    #[tokio::test]
    async fn test_multiple_messages() {
        let (client_half, server_half) = duplex(4096);
        let (_client_read, client_write) = tokio::io::split(client_half);
        let (server_read, _server_write) = tokio::io::split(server_half);

        let mut writer = DapWriter::new(client_write);
        let mut reader = DapReader::new(server_read);

        let messages = vec![
            json!({"seq": 1, "type": "request", "command": "initialize"}),
            json!({"seq": 2, "type": "request", "command": "launch"}),
            json!({"seq": 3, "type": "request", "command": "configurationDone"}),
        ];

        for msg in &messages {
            writer.send(msg).await.unwrap();
        }

        for expected in &messages {
            let received = reader.read_message().await.unwrap();
            assert_eq!(&received, expected);
        }
    }
}
