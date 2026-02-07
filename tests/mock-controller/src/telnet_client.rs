//! Telnet client for sending commands to probe

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::{debug, info};

/// Simple telnet client for sending measurement commands
pub struct TelnetClient {
    stream: TcpStream,
}

impl TelnetClient {
    /// Connect to telnet server
    pub async fn connect(host: &str, port: u16) -> Result<Self> {
        let addr = format!("{}:{}", host, port);
        let stream = TcpStream::connect(&addr).await?;

        info!("Connected to telnet server at {}", addr);

        Ok(Self { stream })
    }

    /// Send a line to the server
    pub async fn send_line(&mut self, line: &str) -> Result<()> {
        debug!("Sending telnet command: {}", line);

        self.stream.write_all(line.as_bytes()).await?;
        self.stream.write_all(b"\n").await?;
        self.stream.flush().await?;

        Ok(())
    }

    /// Read a line from the server
    pub async fn read_line(&mut self) -> Result<String> {
        let mut reader = BufReader::new(&mut self.stream);
        let mut line = String::new();
        reader.read_line(&mut line).await?;

        debug!("Received telnet response: {}", line);

        Ok(line)
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    #[ignore] // Requires running server
    async fn test_telnet_client() {
        // This would require a running telnet server
        // Kept as documentation of the API
    }
}
