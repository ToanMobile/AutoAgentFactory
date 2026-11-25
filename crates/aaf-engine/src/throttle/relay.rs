//! Passes a signal meant for `aaf` on to a heavy command.
//!
//! The command runs in a process group of its own (so it can be stopped and
//! continued as one), which takes it out of the terminal's foreground group:
//! a Ctrl-C on `git commit` whose hook runs `aaf slot -- ./gradlew …` ended
//! `aaf` and left Gradle running. `aaf` cannot catch the signal itself
//! (a handler needs `unsafe`, and blocking the signal for `sigwait` would be
//! inherited by the command). So a small `/bin/sh` stays in `aaf`'s own
//! process group, gets the same signal the terminal sends to that group, and
//! sends it on to the command's group; if the group is still there after a
//! grace period it is killed.
//!
//! A signal sent to the `aaf` process alone (`kill <pid>` from a supervisor
//! or the factory) never reaches the relay, but its effect does: `aaf` dies
//! without saying the command is over, and the pipe it holds closes. The
//! relay takes that as the end of `aaf` and stops the command's group the
//! same way, with SIGTERM (it cannot know which signal ended `aaf`). It
//! cannot tell a SIGKILL from a SIGTERM either, so a SIGKILL of `aaf` alone
//! ends the command too. The command is left running only when the relay is
//! gone as well (a SIGKILL to `aaf`'s whole group, the factory's group kill
//! of a gate): then the slot's orphan record keeps the slot until the
//! command ends, as before.
//!
//! The pipe alone can miss that death: on macOS its write end is marked
//! close-on-exec in a second step, so a child another thread of `aaf`
//! spawns in between holds a copy and the pipe stays open until that child
//! ends (`aaf_fp::output`). A socket pair would not help: its reader too
//! sees the end only once every copy is closed or `aaf` shuts it down, which
//! a dead `aaf` cannot. So an armed relay also asks the kernel every 0.2 s
//! whether `aaf`'s pid still exists (`kill -0`, a shell builtin) and treats
//! its absence as the closed pipe. Two cases still fall back to the
//! pipe: an `aaf` that is a zombie its parent has not collected (`kill -0`
//! answers for a zombie on macOS 27), and a pid already given to another
//! process.
//!
//! The relay acts on a closed pipe only for a group whose leader was a child
//! of `aaf` when it started, so a relay given some other group's id (a test
//! with a made-up id, an id that was free again) never signals it unasked.

use std::io::{self, PipeWriter, Write};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// Seconds between the forwarded signal and SIGKILL.
const GRACE_SECS: u32 = 10;

/// What `aaf` writes when the command is over; anything else, or nothing
/// before the pipe closes, means `aaf` ended first.
const DONE: &[u8] = b"done\n";

/// `$1` is the command's process group, `$2` the grace in seconds, `$3` the
/// most SIGKILL rounds, `$4` the pid of `aaf` (the relay's parent).
///
/// `armed` is set when the group's leader is a child of `aaf`: only then
/// does a closed pipe stop the group. The reader
/// is a shell of its own with a name `ps` shows, so whoever waits for the
/// relay to be ready can see it; it exits 0 only after reading the `done`
/// line. The relay waits for it: a trapped signal ends the `wait` at once,
/// and so does the end of the reader. A signal that came before the reader
/// started skips the wait. When armed, a watcher ends the reader (not done)
/// once `$4` is gone, and the relay ends the watcher when it exits.
///
/// The pre-`SIGTERM` closed-pipe check uses `kill -0 -"$1"` directly (avoiding
/// a full `ps -A` scan before signalling; `SIGTERM` to a zombie-only group is
/// a POSIX no-op). `bounded_ps` caps `/bin/ps -A` with a portable timer that
/// cleans up its own sleep child; if `ps` stalls or fails, `group_live` treats
/// the group as still alive (`return 0`) so the relay never exits early.
///
/// The SIGKILL is repeated until `kill -0` finds the group empty, for at
/// most `$3` rounds a tenth of a second apart: on macOS one SIGKILL to a
/// group misses a member being forked at that instant, which then runs on
/// with the group id (as in `gate::proc::kill_group_until_empty`). The loop
/// stops at the first empty `kill -0` answer, so a group id given to somebody
/// else afterwards is not signalled, and checks `group_live` only sparsely
/// (never spawning bounded `ps` timers on every 0.1 s tick) to catch
/// zombie-only groups. If members are still there at the end it says so on
/// stderr, where the person who pressed Ctrl-C looks.
const SCRIPT: &str = r#"sig=
trap 'sig=INT' INT
trap 'sig=TERM' TERM
trap 'sig=HUP' HUP
armed=
leader_parent=$(/bin/ps -o ppid= -p "$1" 2>/dev/null)
for p in $leader_parent; do leader_parent=$p; break; done
[ -n "$leader_parent" ] && [ "$leader_parent" = "$4" ] && armed=1
exec 3<&0
/bin/sh -c 'IFS= read -r said && [ "$said" = done ]' aaf-relay-reader <&3 >/dev/null 2>&1 &
reader=$!
watcher=
trap '[ -n "$watcher" ] && kill "$watcher" 2>/dev/null; [ -n "$reader" ] && kill "$reader" 2>/dev/null; wait 2>/dev/null' EXIT
if [ -n "$armed" ]; then
  (
    sp=
    last_sp=$reader
    trap '[ -n "$sp" ] && kill "$sp" 2>/dev/null; [ -n "$!" ] && [ "$!" != "$last_sp" ] && kill "$!" 2>/dev/null; exit 0' TERM INT HUP
    while kill -0 "$reader" 2>/dev/null; do
      kill -0 "$4" 2>/dev/null || { kill "$reader" 2>/dev/null; exit 0; }
      /bin/sleep 0.2 &
      sp=$!
      wait "$sp" 2>/dev/null
      last_sp=$sp
      sp=
    done
  ) </dev/null >/dev/null 2>&1 3<&- &
  watcher=$!
fi
bounded_ps() {
  (
    ps_pid=
    t_pid=
    trap '[ -n "$t_pid" ] && kill "$t_pid" 2>/dev/null; [ -n "$ps_pid" ] && kill -KILL "$ps_pid" 2>/dev/null; exit 1' TERM INT HUP
    /bin/ps -A -o pgid=,stat= </dev/null 2>/dev/null 3<&- &
    ps_pid=$!
    (
      sp=
      trap '[ -n "$sp" ] && kill "$sp" 2>/dev/null; [ -n "$!" ] && [ "$!" != "$ps_pid" ] && kill "$!" 2>/dev/null; exit 0' TERM INT HUP
      kill -0 "$ps_pid" 2>/dev/null || exit 0
      /bin/sleep 2 &
      sp=$!
      wait "$sp" 2>/dev/null && kill -KILL "$ps_pid" 2>/dev/null
    ) </dev/null >/dev/null 2>&1 3<&- &
    t_pid=$!
    wait "$ps_pid" 2>/dev/null
    rc=$?
    kill "$t_pid" 2>/dev/null
    wait "$t_pid" 2>/dev/null
    exit "$rc"
  )
}
group_live() {
  kill -0 -"$1" 2>/dev/null || return 1
  out=$(bounded_ps) || return 0
  [ -n "$out" ] || return 0
  printf '%s\n' "$out" | (
    while read -r g s; do
      [ "$g" = "$1" ] || continue
      case "$s" in Z*) ;; *) exit 0 ;; esac
    done
    exit 1
  )
}
[ -n "$sig" ] || wait "$reader"
said=$?
if [ -n "$watcher" ]; then
  kill "$watcher" 2>/dev/null
  wait "$watcher" 2>/dev/null
  watcher=
fi
if [ -n "$sig" ]; then
  kill "$reader" 2>/dev/null
  wait "$reader" 2>/dev/null
fi
reader=
if [ -z "$sig" ]; then
  [ "$said" -ne 0 ] && [ -n "$armed" ] || exit 0
  kill -0 -"$1" 2>/dev/null || exit 0
  sig=TERM
fi
kill -"$sig" -"$1" 2>/dev/null
n=0
grace_ticks=$(($2 * 5))
while [ "$n" -lt "$grace_ticks" ]; do
  kill -0 -"$1" 2>/dev/null || exit 0
  if [ "$n" -eq 1 ] || [ $((n % 5)) -eq 0 ]; then
    group_live "$1" || exit 0
  fi
  /bin/sleep 0.2
  n=$((n+1))
done
n=0
while :; do
  kill -0 -"$1" 2>/dev/null || exit 0
  kill -KILL -"$1" 2>/dev/null
  kill -0 -"$1" 2>/dev/null || exit 0
  n=$((n+1))
  if [ "$n" -eq 2 ] || [ "$n" -eq 5 ] || [ $((n % 10)) -eq 0 ] || [ "$n" -ge "$3" ]; then
    group_live "$1" || exit 0
  fi
  [ "$n" -lt "$3" ] || break
  /bin/sleep 0.1
done
echo "[aaf] process group $1 still had members after $n rounds of SIGKILL; they may still be running (ps -o pid,command -g $1)" >&2
exit 1
"#;

/// Most SIGKILL rounds of the relay, a tenth of a second apart: about the
/// 30 s the gate's own timeout kill goes on for.
const KILL_ROUNDS: u32 = 300;

/// Name the relay shows in the process table.
pub(crate) const RELAY_NAME: &str = "aaf-signal-relay";

pub struct SignalRelay {
    /// Closing it without [`DONE`] is how the relay learns that `aaf`
    /// ended before the command.
    alive: Option<PipeWriter>,
    child: Child,
}

impl SignalRelay {
    /// Starts the relay for the command whose process group is `pgid`.
    pub fn start(pgid: u32) -> io::Result<SignalRelay> {
        SignalRelay::start_with_grace(pgid, GRACE_SECS)
    }

    /// [`SignalRelay::start`] with `grace_secs` between the forwarded
    /// signal and SIGKILL.
    pub(crate) fn start_with_grace(pgid: u32, grace_secs: u32) -> io::Result<SignalRelay> {
        SignalRelay::start_for(pgid, grace_secs, std::process::id())
    }

    /// [`SignalRelay::start_with_grace`] guarding for `owner`, which is
    /// `aaf` itself except in a test that has to end the owner.
    pub(crate) fn start_for(pgid: u32, grace_secs: u32, owner: u32) -> io::Result<SignalRelay> {
        let (reader, writer) = io::pipe()?;
        // No process group of its own: it has to be in the group the
        // terminal signals.
        let child = Command::new("/bin/sh")
            .args([
                "-c",
                SCRIPT,
                RELAY_NAME,
                &pgid.to_string(),
                &grace_secs.to_string(),
                &KILL_ROUNDS.to_string(),
                &owner.to_string(),
            ])
            .stdin(reader)
            .stdout(Stdio::null())
            // `aaf`'s own stderr, for the line of a kill that gave up; every
            // other command of the script is quiet.
            .stderr(Stdio::inherit())
            .spawn()?;
        Ok(SignalRelay {
            alive: Some(writer),
            child,
        })
    }

    /// Tells the relay the command is over and closes the pipe.
    fn say_done(&mut self) {
        if let Some(mut writer) = self.alive.take() {
            // A relay that already ended (after a signal) has closed its
            // end; there is nobody left to tell, which is what this is for.
            let _ = writer.write_all(DONE);
        }
    }

    /// The command is over: lets the relay end and collects it.
    pub fn stop(mut self) {
        self.say_done();
        // It exits as soon as it reads the line; the bound is for a shell
        // that does not, which must not hang the command's result.
        for _ in 0..200 {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        // Still there after a second: end it; the result is unaffected.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for SignalRelay {
    /// Dropped without [`SignalRelay::stop`] (an error path or an unwind in
    /// `aaf`): still an orderly end of the supervision, not the death of
    /// `aaf`, so the command is not stopped for it.
    fn drop(&mut self) {
        self.say_done();
    }
}

#[cfg(test)]
impl SignalRelay {
    pub(crate) fn id(&self) -> u32 {
        self.child.id()
    }

    /// Closes the pipe the way the death of `aaf` does: without the line.
    pub(crate) fn abandon(&mut self) {
        drop(self.alive.take());
    }

    /// Waits up to `limit` for the relay to end by itself; true when it did.
    pub(crate) fn ended_within(&mut self, limit: Duration) -> bool {
        let end = std::time::Instant::now() + limit;
        while std::time::Instant::now() < end {
            if !matches!(self.child.try_wait(), Ok(None)) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }
}
