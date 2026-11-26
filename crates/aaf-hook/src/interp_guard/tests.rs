//! Unit tests for Perl switch values in `inline_payloads` (SEC-084).

use super::{Interp, inline_payloads};
use crate::interp_guard::reads_program_from_stdin;

/// Each row: an argv and the payloads Perl compiles as code from it (with
/// several, their concatenation last).
fn check(rows: &[(&[&str], &[&str])]) {
    for (args, want) in rows {
        let args: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        assert_eq!(inline_payloads(Interp::Perl, &args), *want, "{args:?}");
    }
}

#[test]
fn record_separators_and_config_queries_do_not_shadow_e_flags() {
    check(&[
        (&["-0x1e", "-e", "A"], &["A"]),
        (&["-0x1E", "-E", "A"], &["A"]),
        (&["-0x1de", "-e", "A"], &["A"]),
        // `-0x1e -e` in one argument is `-0 -x1e -e`: no inline program.
        (&["-0x1e -e", "A"], &[]),
        (&["-0777e", "A"], &["A"]),
        // `-0X` is `-0` then `-X`; the following `e` is still `-e`.
        (&["-0Xe", "A"], &["A"]),
        (&["-0x0a", "-ne", "print"], &["print"]),
        (&["-V:package", "-e", "system(1)"], &["system(1)"]),
        (&["-V", "-e", "1"], &["1"]),
    ]);
}

#[test]
fn module_switch_trailing_code_is_a_payload() {
    check(&[
        (&["-MPOSIX;system(1)", "/dev/null"], &[";system(1)"]),
        (&["-M-POSIX;system(1)"], &[";system(1)"]),
        (&["-MPOSIX (system 1)"], &[" (system 1)"]),
        (&["-MPOSIX\nsystem 1"], &["\nsystem 1"]),
        (&["-wMFile::Spec;system(1)"], &[";system(1)"]),
        (&["-M5.010;system(1)"], &[";system(1)"]),
        // `'` joins packages before an identifier, and only there.
        (&["-MFile'Spec;system(1)"], &[";system(1)"]),
        (&["-d:Foo'Bar;system(1)"], &[";system(1)"]),
        (&["-Mv1'Foo;system(1)"], &[";system(1)"]), // `v1'Foo` is a name
        (&["-d:v1'Foo;system(1)"], &[";system(1)"]),
        (&["-Mlib'/x';system(1)"], &["'/x';system(1)"]),
        (&["-MA'\"';system(1)"], &["'\"';system(1)"]),
        (&["-d:PPPort;system(1)"], &[";system(1)"]),
        (&["-dt:-PPPort;system(1)"], &[";system(1)"]),
        (&["-wd=PPPort;system(1)"], &[";system(1)"]),
        (&["-dt=PPPort=x});system(1);({"], &["=x});system(1);({"]),
    ]);
}

#[test]
fn quoted_module_values_count_once_a_string_may_be_open() {
    check(&[
        // An odd run of `\` escapes the `q\0...\0` terminator.
        (
            &["-MFile::Spec=\\", "-MFile::Spec=);system(1);#"],
            &[");system(1);#"],
        ),
        (&["-mA=\\", "-M-A=);system(1)"], &[");system(1)"]),
        (&["-wMA=a\\\\\\", "-mA=x"], &["x"]),
        (
            &["-d:A=x\\", "-MA=});system(1)"],
            &["=x\\", "});system(1)", "=x\\\n});system(1)"],
        ),
        // Pieces in compile order (`-M` before `-e`), then joined.
        (
            &["-e", "\";system(1)", "-MA;\""],
            &[";\"", "\";system(1)", ";\"\n\";system(1)"],
        ),
        (
            &["-MA;\"", "-MA=\";system(1)"],
            &[";\"", "\";system(1)", ";\"\n\";system(1)"],
        ),
        (
            &["-e", "\"", "-e", "\";system(1)"],
            &["\"", "\";system(1)", "\"\n\";system(1)"],
        ),
    ]);
}

#[test]
fn module_names_versions_and_imports_are_not_payloads() {
    check(&[
        (&["-MPOSIX", "-e", "print 1"], &["print 1"]),
        (
            &["-Mstrict", "-M-warnings", "-MTest::More", "script.pl"],
            &[],
        ),
        (
            &["-MList::Util=sum,max", "-mPOSIX=floor", "-e", "1"],
            &["1"],
        ),
        (
            &["-MA=a\\\\", "-MA=x", "-mPOSIX", "-M5.010", "-Mv5.10"],
            &[],
        ),
        (&["-Mv1::Foo", "-Mv1_Foo'Bar", "-Mv5"], &[]), // names, not versions
        // Bare `-M`/`-m` still skip their (rejected) detached argument.
        (&["-M", "POSIX", "-m", "Exporter", "-e", "1"], &["1"]),
        (&["-d", "-d:NYTProf", "-d:Trace=1", "x"], &[]),
        (&["-dt:NYTProf=start=no", "x"], &[]),
        (&["-d:Trace", "-e", "system(1)"], &["system(1)"]),
        (&["-dte", "1"], &["1"]), // `-d -t -e`
    ]);
}

#[test]
fn stdin_program_detection_skips_whole_switch_values() {
    for (line, want) in [
        ("perl -d:BarI script.pl", false),
        ("perl -dt=BarM script.pl", false),
        ("perl -V:foom script.pl", false),
        ("perl '-MPOSIX;BEGIN{exec \"sh\"}'", false),
        ("perl -M5.010 -Mv1::Foo", true),
        ("perl -Mv5.10 -d:Trace=1", true),
        ("perl -MList::Util=sum -d:NYTProf", true),
    ] {
        let script = crate::shell::parse(&format!("{line} <<'EOF'\nx\nEOF"));
        assert_eq!(reads_program_from_stdin(&script.cmds[0]), want, "{line}");
    }
}
