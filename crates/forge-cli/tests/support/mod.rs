//! Bounded child-process lifecycle shared by CLI integration tests.

use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(30);

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn spawn(cmd: &mut Command) -> Self {
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        Self(Some(cmd.spawn().expect("spawn forge")))
    }

    fn wait(mut self, timeout: Duration) -> Output {
        let deadline = Instant::now() + timeout;
        loop {
            let child = self.0.as_mut().expect("guard owns child");
            if child.try_wait().expect("poll forge child").is_some() {
                return self.collect();
            }
            if Instant::now() >= deadline {
                let mut child = self.0.take().expect("guard owns child");
                let _ = child.kill();
                let output = collect_output(child);
                panic!(
                    "forge child exceeded {timeout:?}\nstdout={}\nstderr={}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn collect(&mut self) -> Output {
        collect_output(self.0.take().expect("guard owns child"))
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn collect_output(child: Child) -> Output {
    child.wait_with_output().expect("collect forge output")
}

pub fn output(cmd: &mut Command) -> Output {
    ChildGuard::spawn(cmd).wait(PROCESS_TIMEOUT)
}
