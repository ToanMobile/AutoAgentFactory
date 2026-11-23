use super::super::probe::{
    LoadProbe, Pressure, SystemProbe, load_percent, parse_loadavg, parse_macos_pressure,
    parse_meminfo, parse_psi_memory, parse_vm_stat, sum_group_rss,
};

/// Captured on the owner's Mac (12 CPUs, 24 GB) on 2026-11-22, while three
/// lanes were compiling: `sysctl -n vm.loadavg`.
const MAC_LOADAVG: &str = "{ 40.06 67.96 50.35 }\n";
/// Same capture: `sysctl -n kern.memorystatus_vm_pressure_level`.
const MAC_PRESSURE: &str = "1\n";
/// Same capture: `vm_stat`.
const MAC_VM_STAT: &str = "\
Mach Virtual Memory Statistics: (page size of 16384 bytes)
Pages free:                                     7526.
Pages active:                                 440773.
Pages inactive:                               435572.
Pages speculative:                              4452.
Pages throttled:                                   0.
Pages wired down:                             248510.
Pages purgeable:                                1490.
\"Translation faults\":                      151393085.
Pages copy-on-write:                        14664710.
Pages zero filled:                          41975389.
Pages reactivated:                          24324833.
Pages purged:                                1030049.
File-backed pages:                            248580.
Anonymous pages:                              632217.
Pages stored in compressor:                   819281.
Pages occupied by compressor:                 394845.
Decompressions:                             20522486.
Compressions:                               26052894.
Pageins:                                     6254288.
Pageouts:                                      71867.
Swapins:                                      253579.
Swapouts:                                     596701.
";
/// Same capture: first lines of `ps -A -o pgid=,rss=`, plus a second member
/// of group 96 added by hand to exercise the sum.
const MAC_PS: &str =
    "    1  15392\n   96  16576\n   98   1488\n   99   4768\n  101   9408\n   96    100\n";

// The Linux samples follow the kernel documentation (proc(5), PSI); they
// were not captured on a real Linux host.
const LINUX_LOADAVG: &str = "0.52 0.58 0.59 1/467 12345\n";
const LINUX_PSI_CALM: &str = "\
some avg10=0.00 avg60=0.00 avg300=0.00 total=0
full avg10=0.00 avg60=0.00 avg300=0.00 total=0
";
const LINUX_MEMINFO: &str = "\
MemTotal:       16314560 kB
MemFree:          812344 kB
MemAvailable:    9731052 kB
Buffers:          412300 kB
";

#[test]
fn loadavg_table() {
    let cases: &[(&str, Option<f64>)] = &[
        (MAC_LOADAVG, Some(40.06)),
        (LINUX_LOADAVG, Some(0.52)),
        ("{ 1,50 2,00 3,00 }", Some(1.5)),
        ("0.00 0.00 0.00", Some(0.0)),
        ("", None),
        ("{ }", None),
        ("load: high", None),
        ("-1.0 2.0 3.0", None),
        ("nan 1 1", None),
    ];
    for (text, want) in cases {
        assert_eq!(parse_loadavg(text), *want, "{text:?}");
    }
}

#[test]
fn load_percent_table() {
    assert_eq!(load_percent(40.06, 12), 334);
    assert_eq!(load_percent(0.52, 4), 13);
    assert_eq!(load_percent(6.0, 4), 150);
    // A zero CPU count is treated as one, never a division by zero.
    assert_eq!(load_percent(1.5, 0), 150);
    assert_eq!(load_percent(f64::MAX, 1), u32::MAX);
}

#[test]
fn macos_pressure_table() {
    let cases: &[(&str, Option<Pressure>)] = &[
        (MAC_PRESSURE, Some(Pressure::Normal)),
        ("2\n", Some(Pressure::Warn)),
        ("4", Some(Pressure::Critical)),
        // Unknown levels are unknown, not "probably fine".
        ("3", None),
        ("0", None),
        ("", None),
        ("sysctl: unknown oid", None),
    ];
    for (text, want) in cases {
        assert_eq!(parse_macos_pressure(text), *want, "{text:?}");
    }
}

#[test]
fn vm_stat_table() {
    // (7526 free + 4452 speculative + 435572 inactive + 1490 purgeable) pages.
    assert_eq!(parse_vm_stat(MAC_VM_STAT), Some(449_040 * 16_384));
    let only_free = "Mach Virtual Memory Statistics: (page size of 4096 bytes)\nPages free: 10.\n";
    assert_eq!(parse_vm_stat(only_free), Some(40_960));
    let no_page_size = MAC_VM_STAT.replace("page size of 16384 bytes", "");
    assert_eq!(parse_vm_stat(&no_page_size), None);
    let no_free = MAC_VM_STAT.replace("Pages free:", "Pages gone:");
    assert_eq!(parse_vm_stat(&no_free), None);
    assert_eq!(parse_vm_stat(""), None);
}

#[test]
fn psi_table() {
    let psi = |some: &str, full: &str| {
        format!(
            "some avg10={some} avg60=0.00 avg300=0.00 total=1\n\
             full avg10={full} avg60=0.00 avg300=0.00 total=1\n"
        )
    };
    let parse = |some: &str, full: &str| parse_psi_memory(&psi(some, full));
    assert_eq!(parse_psi_memory(LINUX_PSI_CALM), Some(Pressure::Normal));
    assert_eq!(parse("9.99", "0.00"), Some(Pressure::Normal));
    assert_eq!(parse("10.00", "0.00"), Some(Pressure::Warn));
    assert_eq!(parse("40.00", "4.99"), Some(Pressure::Warn));
    assert_eq!(parse("40.00", "5.00"), Some(Pressure::Critical));
    // Old kernels print only the `some` line.
    let some_only = "some avg10=12.00 avg60=0.00 avg300=0.00 total=1\n";
    assert_eq!(parse_psi_memory(some_only), Some(Pressure::Warn));
    assert_eq!(parse_psi_memory("full avg10=9.00\n"), None);
    assert_eq!(parse_psi_memory(""), None);
}

#[test]
fn meminfo_table() {
    assert_eq!(parse_meminfo(LINUX_MEMINFO), Some(9_731_052 * 1024));
    assert_eq!(parse_meminfo("MemFree: 5 kB\n"), None);
    assert_eq!(parse_meminfo("MemAvailable: lots kB\n"), None);
    // A unit other than kB would silently be off by a factor.
    assert_eq!(parse_meminfo("MemAvailable: 5 MB\n"), None);
}

#[test]
fn group_rss_sums_only_the_group() {
    assert_eq!(sum_group_rss(MAC_PS, 96), Some((16_576 + 100) * 1024));
    assert_eq!(sum_group_rss(MAC_PS, 1), Some(15_392 * 1024));
    // 9 is a prefix of 96, 98 and 99 but names no group.
    assert_eq!(sum_group_rss(MAC_PS, 9), None);
    assert_eq!(sum_group_rss("garbage\n", 1), None);
}

/// A measurement tool that answered and exited is an answer, even while
/// another process still holds its output (a pipe copy that leaked into a
/// child another thread spawned: `aaf_fp::output`).
#[test]
fn another_holder_of_the_output_does_not_lose_a_finished_answer() {
    let dir = tempfile::TempDir::new().unwrap();
    let pid_file = dir.path().join("holder.pid");
    let script = format!(
        "/bin/sleep 30 & echo $! > '{}'; echo out",
        pid_file.display()
    );
    let mut command = std::process::Command::new("/bin/sh");
    command.args(["-c", &script]);
    let finished =
        crate::process::run_with_timeout(command, std::time::Duration::from_secs(60), true);
    // Whatever the outcome, the holder does not outlive the test.
    if let Ok(pid) = std::fs::read_to_string(&pid_file) {
        let _ = std::process::Command::new("/bin/kill")
            .args(["-KILL", pid.trim()])
            .status();
    }
    let finished = finished.unwrap();
    assert!(
        finished.status.is_some_and(|status| status.success()),
        "{:?}",
        finished.status
    );
    assert_eq!(finished.stdout, b"out\n");
}

/// Real run: the probe of this machine answers all three questions.
#[test]
fn system_probe_reads_this_machine() {
    let probe = SystemProbe::detect();
    assert!(probe.cpus >= 1);
    assert!(probe.load_percent().is_some());
    let free = probe.free_memory_bytes().unwrap();
    assert!(free > 0);
    if cfg!(target_os = "macos") {
        // Linux containers often hide /proc/pressure; macOS always answers.
        assert!(probe.pressure().is_some());
        let cpu = probe.cpu_busy_percent();
        assert!(cpu.is_some_and(|pct| pct <= 100), "{cpu:?}");
    }
}

#[test]
fn parse_macos_iostat_cpu_uses_second_sample_and_dynamic_id_column() {
    use super::super::probe::parse_macos_iostat_cpu;

    let clean = "\
      cpu    load average
 us sy id   1m   5m   15m
 33 16 51  11.59 13.72 14.02
 47 15 39  11.59 13.72 14.02
";
    assert_eq!(parse_macos_iostat_cpu(clean), Some(61));

    // Also works when disk columns precede `us sy id`.
    let with_disks = "\
disk0               cpu    load average
    KB/t  tps  MB/s  us sy id   1m   5m   15m
   25.08 1406 34.44  33 16 51  13.87 15.83 14.02
   25.08  693 16.97  42 14 44  13.87 15.83 14.02
";
    assert_eq!(parse_macos_iostat_cpu(with_disks), Some(56));

    // Only 1 sample (since-boot average) or missing header -> None.
    let one_row = " us sy id\n 33 16 51\n";
    assert_eq!(parse_macos_iostat_cpu(one_row), None);
    assert_eq!(parse_macos_iostat_cpu("garbage"), None);
}

#[test]
fn parse_linux_proc_stat_cpu_excludes_idle_and_iowait() {
    use super::super::probe::parse_linux_proc_stat_cpu;

    let before = "cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 50 0 25 400 25 0 0 0 0 0\n";
    // +40 user, +20 sys, +30 idle, +10 iowait => total +100, non-busy +40 => 60% busy
    let after = "cpu  140 0 70 830 60 0 0 0 0 0\n";
    assert_eq!(parse_linux_proc_stat_cpu(before, after), Some(60));
    assert_eq!(parse_linux_proc_stat_cpu(before, before), None);
}
