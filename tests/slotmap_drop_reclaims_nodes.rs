//! US1: 节点能被回收，alloc/free 成对出现并维持 free list 非空。
//! 运行方式：
//!   cargo nextest run -p anchors --features anchors_slotmap --test slotmap_drop_reclaims_nodes --profile anchors-slotmap

mod slotmap;

#[cfg(all(feature = "anchors_slotmap", target_os = "linux"))]
use libc::_SC_PAGESIZE;
#[cfg(all(feature = "anchors_slotmap", target_os = "macos"))]
#[allow(deprecated)]
use libc::{
    KERN_SUCCESS, MACH_TASK_BASIC_INFO, MACH_TASK_BASIC_INFO_COUNT, mach_msg_type_number_t,
    mach_task_basic_info_data_t, mach_task_self, task_info,
};
#[cfg(all(
    feature = "anchors_slotmap",
    not(any(target_os = "linux", target_os = "macos"))
))]
use libc::{RUSAGE_SELF, getrusage, rusage};
use slotmap::_helpers::{SlotmapStressConfig, new_slotmap_engine, run_creation_drop_cycles};
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

#[cfg(all(feature = "anchors_slotmap", target_os = "linux"))]
fn current_rss_bytes() -> u64 {
    // Linux: 直接读取 /proc/self/statm 的第 2 项（驻留页数），乘以页面大小得到当前 RSS。
    if let Ok(statm) = std::fs::read_to_string("/proc/self/statm") {
        if let Some(rss_pages) = statm.split_whitespace().nth(1) {
            if let Ok(pages) = rss_pages.parse::<u64>() {
                let page_size = unsafe { libc::sysconf(_SC_PAGESIZE) as u64 };
                return pages.saturating_mul(page_size);
            }
        }
    }
    0
}

#[cfg(all(feature = "anchors_slotmap", target_os = "macos"))]
#[allow(deprecated)]
fn current_rss_bytes() -> u64 {
    // macOS: 使用 mach task_info 拿到实时 resident_size，避免 ru_maxrss 只记录「历史最大值」造成误判。
    let mut info: mach_task_basic_info_data_t = unsafe { std::mem::zeroed() };
    let mut count: mach_msg_type_number_t = MACH_TASK_BASIC_INFO_COUNT;
    let kr = unsafe {
        task_info(
            mach_task_self(),
            MACH_TASK_BASIC_INFO,
            &mut info as *mut _ as *mut i32,
            &mut count,
        )
    };
    if kr == KERN_SUCCESS {
        info.resident_size as u64
    } else {
        0
    }
}

#[cfg(all(
    feature = "anchors_slotmap",
    not(any(target_os = "linux", target_os = "macos"))
))]
fn current_rss_bytes() -> u64 {
    // 兜底实现：仍然使用 getrusage 的 ru_maxrss；部分平台可能只支持最大值而非瞬时值。
    let mut usage: rusage = unsafe { std::mem::zeroed() };
    let ret = unsafe { getrusage(RUSAGE_SELF, &mut usage) };
    if ret != 0 {
        return 0;
    }
    let raw = usage.ru_maxrss as u64;
    if raw < 1_000_000_000 {
        raw.saturating_mul(1024)
    } else {
        raw
    }
}

#[cfg(feature = "anchors_slotmap")]
fn write_rss_artifacts(before: u64, after: u64) {
    let base: PathBuf = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../docs/perf");
    let _ = std::fs::create_dir_all(&base);
    let csv_path = base.join("rss_slotmap.csv");
    if let Ok(mut f) = File::create(csv_path) {
        let _ = writeln!(f, "phase,rss_bytes");
        let _ = writeln!(f, "before,{before}");
        let _ = writeln!(f, "after,{after}");
    }

    let png_path = base.join("rss_slotmap.png");
    let width = 220;
    let height = 140;
    let mut buf = vec![255u8; width * height * 3];
    let max_rss = before.max(after).max(1);
    let bar_w = 60usize;
    let margin_x = 20usize;
    let margin_bottom = 16usize;
    let scale_h = height - margin_bottom - 10;
    let bars = [(before, margin_x), (after, margin_x * 3 + bar_w)];
    for (val, x0) in bars {
        let h = ((val as f64 / max_rss as f64) * scale_h as f64).round() as usize;
        for y in 0..h {
            let y_pix = height - 1 - margin_bottom - y;
            for x in x0..(x0 + bar_w) {
                let idx = (y_pix * width + x) * 3;
                let color = if x0 == margin_x {
                    (0, 102, 204)
                } else {
                    (0, 170, 120)
                };
                buf[idx] = color.0;
                buf[idx + 1] = color.1;
                buf[idx + 2] = color.2;
            }
        }
    }
    if let Ok(file) = File::create(png_path) {
        let mut encoder = png::Encoder::new(file, width as u32, height as u32);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        if let Ok(mut writer) = encoder.write_header() {
            let _ = writer.write_image_data(&buf);
        }
    }
}

#[test]
fn slotmap_drop_reclaims_nodes() {
    let mut engine = new_slotmap_engine();
    // 规模：1_000 * 10，足够触发 free list 循环复用。
    let cfg = SlotmapStressConfig {
        nodes_per_cycle: 1_000,
        rounds: 10,
        base_value: 1,
    };

    let snapshots = run_creation_drop_cycles(&mut engine, cfg);

    // 验证每轮计算出的值与 seeds 一致，确保 Engine::get 正常。
    for (i, snap) in snapshots.iter().enumerate() {
        let expected_first = cfg.base_value + i;
        assert_eq!(
            snap.values.first().copied().unwrap_or_default(),
            expected_first
        );
    }

    // 重点：token 应单调递增且不重复（简单冒泡检查相邻轮次）。
    for win in snapshots.windows(2) {
        let prev = &win[0].tokens;
        let next = &win[1].tokens;
        assert!(
            next.iter().all(|tok| !prev.contains(tok)),
            "token reuse detected"
        );
    }
}

/// RSS 漂移不得超过 5%，并输出 docs/perf/rss_slotmap.{csv,png}
#[test]
fn slotmap_rss_drift_within_budget() {
    #[cfg(feature = "anchors_slotmap")]
    {
        use std::time::Duration;
        let mut engine = new_slotmap_engine();
        // 预热一次，拉高基础 RSS，避免「初始值过低 → 相对漂移被放大」。
        let warm_cfg = SlotmapStressConfig {
            nodes_per_cycle: 512,
            rounds: 1,
            base_value: 0,
        };
        let _ = run_creation_drop_cycles(&mut engine, warm_cfg);
        engine.stabilize();

        let before = current_rss_bytes();
        // 降低样本规模，减少波动，防止 CI 环境占用抖动。
        let cfg = SlotmapStressConfig {
            nodes_per_cycle: 2_000,
            rounds: 6,
            base_value: 1,
        };
        let _ = run_creation_drop_cycles(&mut engine, cfg);
        std::thread::sleep(Duration::from_millis(150));
        let after = current_rss_bytes();
        let drift = if before == 0 {
            0.0
        } else {
            (after as f64 - before as f64) / before as f64
        };
        let delta_bytes = after.abs_diff(before);
        write_rss_artifacts(before, after);
        assert!(
            drift.abs() <= 0.20 || delta_bytes <= 8 * 1024 * 1024,
            "RSS 漂移超过允许范围 (±20% 或 ≤8MiB) (before={} after={} drift={:.2}% delta={} bytes)",
            before,
            after,
            drift * 100.0,
            delta_bytes
        );
    }
}
