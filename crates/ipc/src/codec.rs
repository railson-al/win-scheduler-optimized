//! Frame reading and writing (task 1.3).
//!
//! A frame is a 4-byte big-endian length followed by a JSON body. The body may not exceed
//! [`MAX_FRAME_BYTES`]; both directions enforce the limit and report it as [`CodecError::FrameTooLarge`].

use std::io;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

/// Largest accepted JSON body, excluding the 4-byte length header.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// `tokio-util` reports a frame cut off by end-of-stream with this message and kind `Other`.
const TRUNCATED_FRAME_MESSAGE: &str = "bytes remaining on stream";

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid JSON in frame: {0}")]
    Json(#[from] serde_json::Error),
    #[error("frame exceeds the 8 MiB limit")]
    FrameTooLarge,
}

pub type FrameReader<R> = FramedRead<R, LengthDelimitedCodec>;
pub type FrameWriter<W> = FramedWrite<W, LengthDelimitedCodec>;

fn codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(MAX_FRAME_BYTES)
        .new_codec()
}

pub fn frame_reader<R: AsyncRead>(reader: R) -> FrameReader<R> {
    FramedRead::new(reader, codec())
}

pub fn frame_writer<W: AsyncWrite>(writer: W) -> FrameWriter<W> {
    FramedWrite::new(writer, codec())
}

/// Reads the next frame. `Ok(None)` means the peer closed the stream between frames.
pub async fn read_frame<R, T>(reader: &mut FrameReader<R>) -> Result<Option<T>, CodecError>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    match reader.next().await {
        None => Ok(None),
        Some(Ok(body)) => Ok(Some(serde_json::from_slice(&body)?)),
        Some(Err(err)) => Err(map_read_error(err)),
    }
}

/// Serializes `value` and writes it as one frame.
pub async fn write_frame<W, T>(writer: &mut FrameWriter<W>, value: &T) -> Result<(), CodecError>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = serde_json::to_vec(value)?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(CodecError::FrameTooLarge);
    }
    writer.send(Bytes::from(body)).await?;
    Ok(())
}

fn map_read_error(err: io::Error) -> CodecError {
    match err.kind() {
        // The length codec's only InvalidData error is an oversized length header.
        io::ErrorKind::InvalidData => CodecError::FrameTooLarge,
        io::ErrorKind::Other if err.to_string() == TRUNCATED_FRAME_MESSAGE => CodecError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "stream ended in the middle of a frame",
        )),
        _ => CodecError::Io(err),
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};
    use tokio::io::{AsyncWriteExt, duplex};

    use super::*;
    use crate::protocol::{ClientFrame, PROTOCOL_VERSION, Request};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Ping {
        seq: u32,
        note: String,
    }

    #[tokio::test]
    async fn round_trips_small_struct() {
        let (a, b) = duplex(64 * 1024);
        let mut writer = frame_writer(a);
        let mut reader = frame_reader(b);
        let sent = Ping {
            seq: 1,
            note: "hello".into(),
        };
        write_frame(&mut writer, &sent).await.unwrap();
        let received: Option<Ping> = read_frame(&mut reader).await.unwrap();
        assert_eq!(received, Some(sent));
    }

    #[tokio::test]
    async fn round_trips_client_frame() {
        let (a, b) = duplex(64 * 1024);
        let mut writer = frame_writer(a);
        let mut reader = frame_reader(b);
        let hello = ClientFrame::Hello {
            protocol_version: PROTOCOL_VERSION,
        };
        let request = ClientFrame::Request {
            id: 9,
            request: Request::GetTask { id: "t1".into() },
        };
        write_frame(&mut writer, &hello).await.unwrap();
        write_frame(&mut writer, &request).await.unwrap();
        assert_eq!(read_frame::<_, ClientFrame>(&mut reader).await.unwrap(), Some(hello));
        assert_eq!(read_frame::<_, ClientFrame>(&mut reader).await.unwrap(), Some(request));
    }

    #[tokio::test]
    async fn several_frames_back_to_back() {
        let (a, b) = duplex(64 * 1024);
        let mut writer = frame_writer(a);
        let mut reader = frame_reader(b);
        for seq in 0..3 {
            let ping = Ping {
                seq,
                note: format!("frame {seq}"),
            };
            write_frame(&mut writer, &ping).await.unwrap();
        }
        drop(writer);
        for seq in 0..3 {
            let ping: Ping = read_frame(&mut reader).await.unwrap().unwrap();
            assert_eq!(ping.seq, seq);
        }
        assert_eq!(read_frame::<_, Ping>(&mut reader).await.unwrap(), None);
    }

    #[tokio::test]
    async fn frame_at_limit_is_accepted() {
        // A JSON string adds two quote bytes, so this body is exactly MAX_FRAME_BYTES long.
        let (a, b) = duplex(2 * MAX_FRAME_BYTES);
        let mut writer = frame_writer(a);
        let mut reader = frame_reader(b);
        let value = "x".repeat(MAX_FRAME_BYTES - 2);
        write_frame(&mut writer, &value).await.unwrap();
        assert_eq!(read_frame::<_, String>(&mut reader).await.unwrap(), Some(value));
    }

    #[tokio::test]
    async fn frame_over_limit_fails_on_write() {
        let (a, _b) = duplex(64 * 1024);
        let mut writer = frame_writer(a);
        let value = "x".repeat(MAX_FRAME_BYTES - 1);
        let err = write_frame(&mut writer, &value).await.unwrap_err();
        assert!(matches!(err, CodecError::FrameTooLarge), "got {err:?}");
    }

    #[tokio::test]
    async fn oversized_header_fails_on_read() {
        let (mut raw, b) = duplex(64 * 1024);
        let mut reader = frame_reader(b);
        let claimed = u32::try_from(MAX_FRAME_BYTES + 1).unwrap();
        raw.write_all(&claimed.to_be_bytes()).await.unwrap();
        let err = read_frame::<_, Ping>(&mut reader).await.unwrap_err();
        assert!(matches!(err, CodecError::FrameTooLarge), "got {err:?}");
    }

    #[tokio::test]
    async fn truncated_frame_is_unexpected_eof() {
        let (mut raw, b) = duplex(64 * 1024);
        let mut reader = frame_reader(b);
        raw.write_all(&10u32.to_be_bytes()).await.unwrap();
        raw.write_all(b"abc").await.unwrap();
        drop(raw);
        match read_frame::<_, Ping>(&mut reader).await.unwrap_err() {
            CodecError::Io(err) => assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof),
            other => panic!("expected Io(UnexpectedEof), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_json_is_json_error() {
        let (mut raw, b) = duplex(64 * 1024);
        let mut reader = frame_reader(b);
        let body = b"not json";
        raw.write_all(&u32::try_from(body.len()).unwrap().to_be_bytes())
            .await
            .unwrap();
        raw.write_all(body).await.unwrap();
        let err = read_frame::<_, Ping>(&mut reader).await.unwrap_err();
        assert!(matches!(err, CodecError::Json(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn clean_eof_is_none() {
        let (a, b) = duplex(64 * 1024);
        let mut reader = frame_reader(b);
        drop(a);
        assert_eq!(read_frame::<_, Ping>(&mut reader).await.unwrap(), None);
    }
}
