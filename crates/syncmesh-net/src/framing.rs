//! Length-prefixed framing for postcard-encoded `Frame`s on a QUIC stream.
//!
//! A single bidirectional QUIC stream carries many control frames over its
//! lifetime. QUIC guarantees reliable, ordered bytes; it does not preserve
//! message boundaries. We therefore prepend each frame with a 4-byte
//! big-endian unsigned length.
//!
//! The cap (`MAX_FRAME_BYTES`) is set high enough for any realistic control or
//! chat frame and low enough to keep a malicious peer from asking us to
//! allocate gigabytes. Heartbeats do not use this path — they travel as QUIC
//! datagrams (see `peer.rs`).

use syncmesh_core::{DecodeError, Frame};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Upper bound on a single framed message. Chat messages are the only variable
/// payload; 64 KiB is ample and ~500x our typical control frame.
pub const MAX_FRAME_BYTES: u32 = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FrameSendError {
    #[error("frame exceeds max size: {0} > {MAX_FRAME_BYTES}")]
    TooLarge(usize),
    #[error("stream write failed: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum FrameRecvError {
    #[error("stream closed before a frame was fully read")]
    Eof,
    #[error("advertised length {0} exceeds max {MAX_FRAME_BYTES}")]
    OversizedFrame(u32),
    #[error("stream read failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame decode failed: {0}")]
    Decode(#[from] DecodeError),
}

/// Write a single `Frame` to a stream. The write is one length prefix followed
/// by one postcard payload; callers are responsible for flushing if required.
pub async fn write_frame<W>(w: &mut W, frame: &Frame) -> Result<(), FrameSendError>
where
    W: AsyncWriteExt + Unpin,
{
    let payload = frame.encode();
    if payload.len() > MAX_FRAME_BYTES as usize {
        return Err(FrameSendError::TooLarge(payload.len()));
    }
    // `as u32` is safe because we just checked the bound above.
    let len = u32::try_from(payload.len()).expect("checked above");
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&payload).await?;
    Ok(())
}

/// Read a single `Frame`. Blocks until a full frame is available, or returns
/// `Eof` if the stream ends cleanly between frames.
pub async fn read_frame<R>(r: &mut R) -> Result<Frame, FrameRecvError>
where
    R: AsyncReadExt + Unpin,
{
    let mut len_buf = [0u8; 4];
    // Distinguish clean EOF (zero bytes read before any length byte) from a
    // short read mid-prefix, which is a real truncation error.
    let first = r.read(&mut len_buf[..1]).await?;
    if first == 0 {
        return Err(FrameRecvError::Eof);
    }
    // At least one length byte is in hand — the remaining three must follow.
    r.read_exact(&mut len_buf[1..]).await?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(FrameRecvError::OversizedFrame(len));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    Ok(Frame::decode(&buf)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use syncmesh_core::{ChatMessage, ControlAction, ControlEvent, NodeId};
    use tokio::io::duplex;

    fn sample_ctrl() -> Frame {
        Frame::Control(ControlEvent {
            origin: NodeId::from_bytes([1u8; 32]),
            origin_ts_ms: 12_345,
            seq: 7,
            action: ControlAction::Pause { media_pos_ms: 1_000 },
        })
    }

    #[tokio::test]
    async fn round_trip_single_frame() {
        let (mut a, mut b) = duplex(4096);
        let sent = sample_ctrl();
        write_frame(&mut a, &sent).await.unwrap();
        let got = read_frame(&mut b).await.unwrap();
        assert_eq!(got, sent);
    }

    #[tokio::test]
    async fn round_trip_many_frames_in_order() {
        let (mut a, mut b) = duplex(64 * 1024);
        let frames: Vec<Frame> = (0u8..32)
            .map(|i| {
                Frame::Chat(ChatMessage {
                    origin: NodeId::from_bytes([i; 32]),
                    origin_ts_ms: u64::from(i),
                    text: format!("msg-{i}"),
                })
            })
            .collect();
        let writer = {
            let frames = frames.clone();
            tokio::spawn(async move {
                for f in &frames {
                    write_frame(&mut a, f).await.unwrap();
                }
            })
        };
        for expected in &frames {
            let got = read_frame(&mut b).await.unwrap();
            assert_eq!(&got, expected);
        }
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn clean_eof_is_distinguished_from_truncation() {
        let (a, mut b) = duplex(64);
        drop(a);
        let err = read_frame(&mut b).await.unwrap_err();
        assert!(matches!(err, FrameRecvError::Eof));
    }

    #[tokio::test]
    async fn truncated_length_prefix_is_io_error() {
        let (mut a, mut b) = duplex(64);
        a.write_all(&[0x00, 0x00]).await.unwrap();
        drop(a);
        let err = read_frame(&mut b).await.unwrap_err();
        assert!(matches!(err, FrameRecvError::Io(_)));
    }

    #[tokio::test]
    async fn rejects_oversized_advertised_length() {
        let (mut a, mut b) = duplex(64);
        let huge = (MAX_FRAME_BYTES + 1).to_be_bytes();
        a.write_all(&huge).await.unwrap();
        let err = read_frame(&mut b).await.unwrap_err();
        assert!(matches!(err, FrameRecvError::OversizedFrame(_)));
    }
}
