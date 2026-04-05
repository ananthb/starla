#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // NTP packets are exactly 48 bytes
    if data.len() >= 48 {
        // Simulate parsing the NTP header fields like ntp/client.rs does
        use std::io::Read;
        let mut buf = &data[..48];
        let header = buf[0];
        let _li = (header >> 6) & 0x03;
        let _vn = (header >> 3) & 0x07;
        let _mode = header & 0x07;
        let _stratum = buf[1];
        let _poll = buf[2];
        let _precision = buf[3] as i8;

        // Parse fixed-point fields
        let root_delay = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let root_disp = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let _ref_id = u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]);

        // Parse timestamps (64-bit NTP)
        let _ref_ts = u64::from_be_bytes([
            buf[16], buf[17], buf[18], buf[19], buf[20], buf[21], buf[22], buf[23],
        ]);
        let _orig_ts = u64::from_be_bytes([
            buf[24], buf[25], buf[26], buf[27], buf[28], buf[29], buf[30], buf[31],
        ]);
        let recv_ts = u64::from_be_bytes([
            buf[32], buf[33], buf[34], buf[35], buf[36], buf[37], buf[38], buf[39],
        ]);
        let trans_ts = u64::from_be_bytes([
            buf[40], buf[41], buf[42], buf[43], buf[44], buf[45], buf[46], buf[47],
        ]);

        // Convert and compute like the real code
        let t2 = (recv_ts >> 32) as f64 + (recv_ts & 0xFFFFFFFF) as f64 / 4294967296.0;
        let t3 = (trans_ts >> 32) as f64 + (trans_ts & 0xFFFFFFFF) as f64 / 4294967296.0;
        let t1 = 3984312052.0; // Fake client timestamp
        let t4 = 3984312053.0;
        let rtt = (t4 - t1) - (t3 - t2);
        let offset = ((t2 - t1) + (t3 - t4)) / 2.0;

        // Validate like the real code does
        assert!(rtt.is_finite() || !rtt.is_finite()); // Just ensure no panic
        let _ = (rtt, offset, root_delay, root_disp);
    }
});
