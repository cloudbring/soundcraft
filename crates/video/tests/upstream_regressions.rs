#[cfg(test)]
mod tests {
    use soundcraft_video::bitstream::{self, BitWriter};
    use soundcraft_video::h264::{
        Result,
        params::{Pps, Sps},
        slice::*,
    };

    fn sps_pps() -> (Sps, Pps) {
        let mut writer = BitWriter::new();
        writer.write_bits(66, 8);
        writer.write_bits(0xC0, 8);
        writer.write_bits(30, 8);
        writer.write_ue(0);
        writer.write_ue(0);
        writer.write_ue(2);
        writer.write_ue(1);
        writer.write_bit(false);
        writer.write_ue(10);
        writer.write_ue(8);
        writer.write_bit(true);
        writer.write_bit(true);
        writer.write_bit(false);
        writer.write_bit(false);
        writer.rbsp_trailing();
        let sps = Sps::parse(&writer.finish()).unwrap();
        let mut writer = BitWriter::new();
        writer.write_ue(0);
        writer.write_ue(0);
        writer.write_bit(false);
        writer.write_bit(false);
        writer.write_ue(0);
        writer.write_ue(0);
        writer.write_ue(0);
        writer.write_bit(false);
        writer.write_bits(0, 2);
        writer.write_se(0);
        writer.write_se(0);
        writer.write_se(0);
        writer.write_bit(false);
        writer.write_bit(false);
        writer.write_bit(false);
        writer.rbsp_trailing();
        let pps = Pps::parse(&writer.finish(), &[Some(sps.clone())]).unwrap();
        (sps, pps)
    }

    /// Parse a non-IDR slice header of `slice_type` whose override flag is set with the given counts.
    fn parse_override(slice_type: u32, l0_minus1: u32, l1_minus1: u32, qp_delta: i32) -> Result<SliceHeader> {
        let (sps, pps) = sps_pps();
        let mut writer = BitWriter::new();
        writer.write_ue(0);
        writer.write_ue(slice_type);
        writer.write_ue(0);
        writer.write_bits(0, 4);
        if slice_type == 1 {
            writer.write_bit(true);
        }
        writer.write_bit(true);
        writer.write_ue(l0_minus1);
        if slice_type == 1 {
            writer.write_ue(l1_minus1);
        }
        writer.write_bit(false);
        if slice_type == 1 {
            writer.write_bit(false);
        }
        writer.write_bit(false);
        writer.write_se(qp_delta);
        writer.rbsp_trailing();
        let nal = NalHeader { nal_ref_idc: 1, nal_unit_type: 1 };
        SliceHeader::parse(&writer.finish(), nal, |_| Ok((&pps, &sps))).map(|(header, _, _)| header)
    }

    #[test]
    fn huge_num_ref_idx_override_is_invalid() {
        assert!(matches!(parse_override(0, u32::MAX, 0, 0), Err(soundcraft_video::h264::Error::Bitstream(bitstream::BitError::InvalidExpGolomb))));
        assert!(matches!(parse_override(1, 0, u32::MAX, 0), Err(soundcraft_video::h264::Error::Bitstream(bitstream::BitError::InvalidExpGolomb))));
        assert!(matches!(parse_override(0, 16, 0, 0), Err(soundcraft_video::h264::Error::Invalid(_))));
        assert_eq!(parse_override(0, 15, 0, 0).unwrap().num_ref_idx_active, [16, 0]);
    }

    #[test]
    fn huge_slice_qp_delta_is_invalid() {
        assert!(matches!(parse_override(0, 0, 0, i32::MAX), Err(soundcraft_video::h264::Error::Invalid(_))));
    }

    #[test]
    fn poc0_huge_bottom_delta_does_not_overflow() {
        let (mut sps, _) = sps_pps();
        sps.pic_order_cnt_type = 0;
        sps.log2_max_poc_lsb = 4;
        let mut sh = parse_override(0, 0, 0, 0).unwrap();
        sh.pic_order_cnt_lsb = 1;
        sh.delta_pic_order_cnt_bottom = i32::MAX;
        let poc = PocState::default().compute(&sh, &sps);
        assert_eq!(poc.top, 1);
        assert_eq!(poc.bottom, i32::MAX);
    }
}
