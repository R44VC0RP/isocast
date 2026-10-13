//! FLV tag bodies, which RTMP carries as its audio and video messages
//! (Adobe's "Video File Format Specification", version 10, annex E), and
//! the codec configurations they start with: the AVC decoder configuration
//! record (ISO/IEC 14496-15) and AAC's AudioSpecificConfig (ISO/IEC
//! 14496-3).

use super::amf::Amf;
use crate::ParameterSets;

/// FLV video: frame type (1 keyframe, 2 inter frame) and codec 7 (AVC).
const AVC_KEYFRAME: u8 = 0x17;
const AVC_INTER: u8 = 0x27;
/// FLV audio: format 10 (AAC), "44 kHz" (AAC always says so), 16-bit,
/// stereo; the real format is in the AudioSpecificConfig.
const AAC_STEREO: u8 = 0xaf;
const AAC_MONO: u8 = 0xae;

/// A video message announcing the H.264 configuration.
pub(crate) fn avc_sequence_header(record: &[u8]) -> Vec<u8> {
    let mut body = vec![AVC_KEYFRAME, 0, 0, 0, 0];
    body.extend_from_slice(record);
    body
}

/// A video message with one frame's NAL units (each after a 4-byte
/// length), shown `composition_ms` after it is decoded.
pub(crate) fn avc_frame(keyframe: bool, composition_ms: i32, nal_units: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(5 + nal_units.len());
    body.push(if keyframe { AVC_KEYFRAME } else { AVC_INTER });
    body.push(1);
    body.extend_from_slice(&composition_ms.to_be_bytes()[1..]);
    body.extend_from_slice(nal_units);
    body
}

/// The AVCDecoderConfigurationRecord for `sets`, with 4-byte NAL lengths.
pub(crate) fn avc_decoder_configuration(sets: &ParameterSets) -> Option<Vec<u8>> {
    let sps = sets.sps.first()?;
    if sps.len() < 4 || sets.pps.is_empty() {
        return None;
    }
    // The SPS's NAL header byte, then profile_idc, the constraint flags
    // and level_idc.
    let (profile, compatibility, level) = (sps[1], sps[2], sps[3]);
    let mut record = vec![1, profile, compatibility, level, 0xfc | 3];
    record.push(0xe0 | sets.sps.len().min(31) as u8);
    for sps in sets.sps.iter().take(31) {
        record.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        record.extend_from_slice(sps);
    }
    record.push(sets.pps.len().min(255) as u8);
    for pps in sets.pps.iter().take(255) {
        record.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        record.extend_from_slice(pps);
    }
    // High profiles add the chroma format and bit depths: 4:2:0 and 8-bit,
    // which is all the encoder makes, and no SPS extensions.
    if matches!(profile, 100 | 110 | 122 | 144) {
        record.extend_from_slice(&[0xfc | 1, 0xf8, 0xf8, 0]);
    }
    Some(record)
}

/// An audio message announcing the AAC configuration.
pub(crate) fn aac_sequence_header(config: &[u8], channels: u32) -> Vec<u8> {
    let mut body = vec![aac_flags(channels), 0];
    body.extend_from_slice(config);
    body
}

/// An audio message with one raw AAC frame.
pub(crate) fn aac_frame(frame: &[u8], channels: u32) -> Vec<u8> {
    let mut body = Vec::with_capacity(2 + frame.len());
    body.extend_from_slice(&[aac_flags(channels), 1]);
    body.extend_from_slice(frame);
    body
}

fn aac_flags(channels: u32) -> u8 {
    if channels == 1 { AAC_MONO } else { AAC_STEREO }
}

/// AAC-LC's AudioSpecificConfig: object type 2, the sampling frequency
/// index, the channel configuration, and 1024-sample frames.
pub(crate) fn audio_specific_config(sample_rate: u32, channels: u32) -> [u8; 2] {
    const RATES: [u32; 13] = [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
    ];
    let index = RATES.iter().position(|r| *r == sample_rate).unwrap_or(3) as u16;
    let config: u16 = (2 << 11) | (index << 7) | ((channels as u16 & 0xf) << 3);
    config.to_be_bytes()
}

/// What `@setDataFrame onMetaData` says about the stream.
pub(crate) struct Metadata {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub video_kbps: u32,
    pub audio_kbps: u32,
    pub sample_rate: u32,
    pub channels: u32,
}

/// The data message that describes the stream to the server.
pub(crate) fn metadata(meta: &Metadata) -> Vec<u8> {
    let number = |n: u32| Amf::Number(f64::from(n));
    let properties = vec![
        ("duration".to_string(), Amf::Number(0.0)),
        ("fileSize".to_string(), Amf::Number(0.0)),
        ("width".to_string(), number(meta.width)),
        ("height".to_string(), number(meta.height)),
        ("videocodecid".to_string(), Amf::Number(7.0)),
        ("videodatarate".to_string(), number(meta.video_kbps)),
        ("framerate".to_string(), number(meta.fps)),
        ("audiocodecid".to_string(), Amf::Number(10.0)),
        ("audiodatarate".to_string(), number(meta.audio_kbps)),
        ("audiosamplerate".to_string(), number(meta.sample_rate)),
        ("audiosamplesize".to_string(), Amf::Number(16.0)),
        ("audiochannels".to_string(), number(meta.channels)),
        ("stereo".to_string(), Amf::Boolean(meta.channels == 2)),
        (
            "encoder".to_string(),
            Amf::String(format!("Isocast {}", env!("CARGO_PKG_VERSION"))),
        ),
    ];
    super::amf::encode(&[
        Amf::string("@setDataFrame"),
        Amf::string("onMetaData"),
        Amf::EcmaArray(properties),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aac_lc_config_for_48_khz_stereo_is_0x1190() {
        assert_eq!(audio_specific_config(48_000, 2), [0x11, 0x90]);
        assert_eq!(audio_specific_config(44_100, 1), [0x12, 0x08]);
        assert_eq!(aac_sequence_header(&[0x11, 0x90], 2), [0xaf, 0, 0x11, 0x90]);
    }

    #[test]
    fn avc_record_carries_profile_level_and_parameter_sets() {
        let sets = ParameterSets {
            sps: vec![vec![0x67, 100, 0, 40, 0xac]],
            pps: vec![vec![0x68, 0xee, 0x3c, 0x80]],
        };
        let record = avc_decoder_configuration(&sets).unwrap();
        assert_eq!(&record[..6], &[1, 100, 0, 40, 0xff, 0xe1]);
        assert_eq!(&record[6..8], &[0, 5]);
        assert_eq!(&record[8..13], &sets.sps[0][..]);
        assert_eq!(&record[13..16], &[1, 0, 4]);
        assert_eq!(&record[16..20], &sets.pps[0][..]);
        assert_eq!(&record[20..], &[0xfd, 0xf8, 0xf8, 0]);
        assert!(avc_decoder_configuration(&ParameterSets::default()).is_none());
    }

    #[test]
    fn frames_carry_type_and_composition_time() {
        assert_eq!(avc_frame(true, 0, &[9]), [0x17, 1, 0, 0, 0, 9]);
        assert_eq!(avc_frame(false, -1, &[]), [0x27, 1, 0xff, 0xff, 0xff]);
    }
}
