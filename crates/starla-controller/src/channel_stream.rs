//! Async stream adapter for SSH channels
//!
//! Bridges a `russh::Channel<Msg>` into a standard `DuplexStream` that
//! implements `AsyncRead + AsyncWrite`, usable anywhere that expects a
//! tokio async stream.

use russh::client::Msg;
use russh::{Channel, ChannelMsg};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tracing::{debug, trace};

/// Create a bidirectional async stream from an SSH channel.
///
/// Returns a `DuplexStream` that can be used as a normal async stream.
/// A background task bridges data between the SSH channel and the stream.
/// The task exits when either side closes.
pub fn channel_to_stream(mut channel: Channel<Msg>) -> DuplexStream {
    let (app_side, bridge_side) = tokio::io::duplex(64 * 1024);
    let (mut bridge_read, mut bridge_write) = tokio::io::split(bridge_side);

    tokio::spawn(async move {
        let mut local_buf = [0u8; 8192];

        loop {
            tokio::select! {
                biased;

                // SSH channel -> app
                msg = channel.wait() => {
                    match msg {
                        Some(ChannelMsg::Data { data }) => {
                            trace!("SSH channel -> stream: {} bytes", data.len());
                            if bridge_write.write_all(&data).await.is_err() {
                                break;
                            }
                            let _ = bridge_write.flush().await;
                        }
                        Some(ChannelMsg::Eof) | None => {
                            debug!("SSH channel closed");
                            let _ = bridge_write.shutdown().await;
                            break;
                        }
                        _ => {} // ignore other messages
                    }
                }

                // App -> SSH channel
                result = bridge_read.read(&mut local_buf) => {
                    match result {
                        Ok(0) => {
                            debug!("Stream closed by app");
                            let _ = channel.eof().await;
                            break;
                        }
                        Ok(n) => {
                            trace!("Stream -> SSH channel: {} bytes", n);
                            if channel.data(&local_buf[..n]).await.is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        }

        // Ensure SSH channel is closed when bridge exits
        let _ = channel.eof().await;
        debug!("Channel stream bridge ended");
    });

    app_side
}
