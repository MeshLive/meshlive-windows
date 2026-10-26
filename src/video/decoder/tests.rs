use super::*;

fn header() -> VideoHeader {
    VideoHeader {
        width: 64,
        height: 64,
        fps: 5,
        codec: 2,
        quarter_turns: 0,
        opus: false,
    }
}
#[test]
fn native_input_buffers_keep_bytes_time_and_cleanpoint_with_checked_limits() {
    let (gpu, _, _) = device(true, true).unwrap();
    let _owner = DecodeDevice::new(&gpu).unwrap();
    let packet = [1, 2, 3, 4, 5];
    for (pts, key) in [(-7, false), (200000, true)] {
        let sample = input_sample(&packet, pts, key, 5).unwrap();
        unsafe {
            assert_eq!(sample.GetSampleTime().unwrap(), pts * 10);
            assert_eq!(sample.GetSampleDuration().unwrap(), 2000000);
            assert_eq!(
                sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap(),
                key as u32
            );
            let buffer = sample.GetBufferByIndex(0).unwrap();
            let mut bytes = std::ptr::null_mut();
            let mut count = 0;
            buffer.Lock(&mut bytes, None, Some(&mut count)).unwrap();
            let actual = std::slice::from_raw_parts(bytes, count as usize).to_vec();
            buffer.Unlock().unwrap();
            assert_eq!(actual, packet);
        }
    }
    for (bytes, pts, fps) in [
        (&[][..], 0, 5),
        (&packet[..], i64::MAX, 5),
        (&packet[..], i64::MIN, 5),
        (&packet[..], 0, 0),
        (&packet[..], 0, 121),
    ] {
        assert!(matches!(
            input_sample(bytes, pts, false, fps),
            Err(DecodeError::Input)
        ));
    }
    assert!(matches!(
        input_sample(&vec![0; MAX_PAYLOAD + 1], 0, false, 5),
        Err(DecodeError::Input)
    ));
}

#[test]
fn native_output_metadata_preserves_unknown_values_and_reports_explicit_sdr_tags() {
    let (gpu, _, _) = device(true, true).unwrap();
    let _owner = DecodeDevice::new(&gpu).unwrap();
    let value = input_type(header(), MFVideoFormat_NV12).unwrap();
    let unknown = format(&value, header()).unwrap();
    assert!(unknown.colour.is_none() && unknown.range.is_none() && unknown.chroma.is_none());
    unsafe {
        value
            .SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)
            .unwrap();
        value
            .SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)
            .unwrap();
        value
            .SetUINT32(
                &MF_MT_VIDEO_CHROMA_SITING,
                (MFVideoChromaSubsampling_MPEG2.0 | MFVideoChromaSubsampling_ProgressiveChroma.0)
                    as u32,
            )
            .unwrap();
    }
    let known = format(&value, header()).unwrap();
    assert!(matches!(known.colour, Some(Colour::Bt709)));
    assert!(matches!(known.range, Some(Range::Limited)));
    assert!(matches!(known.chroma, Some(ChromaLocation::Left)));
    for (key, number) in [
        (MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT2020_10.0 as u32),
        (MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_48_208.0 as u32),
        (
            MF_MT_VIDEO_CHROMA_SITING,
            MFVideoChromaSubsampling_DV_PAL.0 as u32,
        ),
        (MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_2084.0 as u32),
    ] {
        let value = input_type(header(), MFVideoFormat_NV12).unwrap();
        unsafe {
            value.SetUINT32(&key, number).unwrap();
        }
        assert!(matches!(
            format(&value, header()),
            Err(DecodeError::Unsupported)
        ));
    }
}

#[test]
fn native_aperture_padding_and_declared_picture_caps_are_checked_without_guessing() {
    let (gpu, _, _) = device(true, true).unwrap();
    let _owner = DecodeDevice::new(&gpu).unwrap();
    let value = input_type(header(), MFVideoFormat_NV12).unwrap();
    let mut area = MFVideoArea {
        OffsetX: MFOffset { value: 8, fract: 0 },
        OffsetY: MFOffset { value: 4, fract: 0 },
        Area: windows::Win32::Foundation::SIZE { cx: 64, cy: 48 },
    };
    unsafe {
        value
            .SetUINT64(&MF_MT_FRAME_SIZE, 128u64 << 32 | 128)
            .unwrap();
        value
            .SetBlob(
                &MF_MT_MINIMUM_DISPLAY_APERTURE,
                std::slice::from_raw_parts(
                    (&area as *const MFVideoArea).cast(),
                    std::mem::size_of_val(&area),
                ),
            )
            .unwrap();
    }
    let crop = format(&value, header()).unwrap().crop;
    assert_eq!(
        [crop.left, crop.top, crop.width, crop.height],
        [8, 4, 64, 48]
    );
    for (left, fract, width) in [(8, 1, 64), (-1, 0, 64), (100, 0, 64), (8, 0, 65), (8, 0, 0)] {
        area.OffsetX.value = left;
        area.OffsetX.fract = fract;
        area.Area.cx = width;
        unsafe {
            value
                .SetBlob(
                    &MF_MT_MINIMUM_DISPLAY_APERTURE,
                    std::slice::from_raw_parts(
                        (&area as *const MFVideoArea).cast(),
                        std::mem::size_of_val(&area),
                    ),
                )
                .unwrap();
        }
        assert!(format(&value, header()).is_err());
    }
    unsafe {
        value.DeleteItem(&MF_MT_MINIMUM_DISPLAY_APERTURE).unwrap();
    }
    assert!(matches!(format(&value, header()), Err(DecodeError::Format)));
    unsafe {
        value
            .SetUINT64(&MF_MT_FRAME_SIZE, 4097u64 << 32 | 64)
            .unwrap();
    }
    assert!(matches!(format(&value, header()), Err(DecodeError::Format)));
    unsafe {
        value.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_P010).unwrap();
    }
    assert!(matches!(
        format(&value, header()),
        Err(DecodeError::Unsupported)
    ));
}
