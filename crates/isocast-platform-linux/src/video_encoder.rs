//! Select hardware before opening an output; never switch codecs mid-file.

use std::time::Duration;

use anyhow::Result;
use isocast_engine::{EncodedVideo, LiveVideoSpec, Time, VideoEncoder};

use crate::{encoder, frames::Frame, nvenc};

pub enum Encoder {
    Nvenc(Box<nvenc::Encoder>),
    Software(Box<encoder::Encoder>),
}

impl Encoder {
    pub fn new(spec: &LiveVideoSpec) -> Result<Self> {
        match nvenc::Encoder::new(spec) {
            Ok(encoder) => Ok(Self::Nvenc(Box::new(encoder))),
            Err(error) => {
                isocast_diagnostics::output::deferred(
                    "info",
                    module_path!(),
                    format!("NVENC unavailable; using OpenH264: {error:#}"),
                );
                encoder::Encoder::new(spec).map(|encoder| Self::Software(Box::new(encoder)))
            }
        }
    }

    pub(crate) fn recording(width: u32, height: u32, fps: u32, quality: u8) -> Result<Self> {
        match nvenc::Encoder::recording(width, height, fps, quality) {
            Ok(encoder) => Ok(Self::Nvenc(Box::new(encoder))),
            Err(error) => {
                isocast_diagnostics::output::deferred(
                    "info",
                    module_path!(),
                    format!("NVENC unavailable; using OpenH264: {error:#}"),
                );
                encoder::Encoder::recording(width, height, fps, quality)
                    .map(|encoder| Self::Software(Box::new(encoder)))
            }
        }
    }

    pub(crate) fn decoder_config(&self) -> Result<Vec<u8>> {
        match self {
            Self::Nvenc(encoder) => encoder.decoder_config(),
            Self::Software(encoder) => encoder.decoder_config(),
        }
    }

    pub(crate) fn hardware_accelerated(&self) -> bool {
        matches!(self, Self::Nvenc(_))
    }
}

impl VideoEncoder for Encoder {
    type Frame = Frame;

    fn encode(&mut self, frame: &Frame, pts: Time, keyframe: bool) -> Result<()> {
        match self {
            Self::Nvenc(encoder) => encoder.encode(frame, pts, keyframe),
            Self::Software(encoder) => encoder.encode(frame, pts, keyframe),
        }
    }

    fn receive(&mut self) -> Result<Option<EncodedVideo>> {
        match self {
            Self::Nvenc(encoder) => encoder.receive(),
            Self::Software(encoder) => encoder.receive(),
        }
    }

    fn receive_timeout(&mut self, timeout: Duration) -> Result<Option<EncodedVideo>> {
        match self {
            Self::Nvenc(encoder) => encoder.receive_timeout(timeout),
            Self::Software(encoder) => encoder.receive_timeout(timeout),
        }
    }

    fn dropped(&self) -> u64 {
        match self {
            Self::Nvenc(encoder) => encoder.dropped(),
            Self::Software(encoder) => encoder.dropped(),
        }
    }

    fn rate_uncertain(&self) -> bool {
        match self {
            Self::Nvenc(encoder) => encoder.rate_uncertain(),
            Self::Software(encoder) => encoder.rate_uncertain(),
        }
    }

    fn set_rate(&mut self, fps: u32, bitrate_kbps: u32) -> Result<()> {
        match self {
            Self::Nvenc(encoder) => encoder.set_rate(fps, bitrate_kbps),
            Self::Software(encoder) => encoder.set_rate(fps, bitrate_kbps),
        }
    }
}
