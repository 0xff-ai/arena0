//! Length-prefixed JSON framing for the daemon socket: a 4-byte big-endian length
//! followed by the JSON body. Gated behind the `io` feature so pure-type consumers
//! do not pull tokio.

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Maximum frame size. Generous: `program.import` carries raw wasm and a receipt
/// carries a full trace, both JSON-encoded.
pub const MAX_FRAME_BYTES: u32 = 128 * 1024 * 1024;

/// Write one length-prefixed JSON frame and flush.
pub async fn write_frame<W, T>(w: &mut W, msg: &T) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize + ?Sized,
{
    let body = serde_json::to_vec(msg).map_err(invalid_data)?;
    let len =
        u32::try_from(body.len()).map_err(|_| std::io::Error::other("frame exceeds u32 length"))?;
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::other("frame exceeds cap"));
    }
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await
}

/// Read one length-prefixed JSON frame. Returns `Ok(None)` on a clean EOF at a
/// frame boundary (the peer closed the connection).
pub async fn read_frame<R, T>(r: &mut R) -> std::io::Result<Option<T>>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::other("frame exceeds cap"));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(invalid_data)
}

fn invalid_data(e: serde_json::Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, e)
}
