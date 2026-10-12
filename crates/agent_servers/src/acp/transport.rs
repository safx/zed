use super::debug_log::{AcpDebugLog, AcpDebugMessageDirection};
use anyhow::{Context as _, Result};
use futures::{
    AsyncBufReadExt as _, AsyncWriteExt as _, FutureExt as _, Sink, StreamExt as _,
    future::BoxFuture, io::BufReader, stream::BoxStream,
};
use gpui::{AsyncApp, Entity, Subscription};
use project::{Project, agent_server_store::AgentServerCommand};
use remote::remote_client::Interactive;
use std::{
    cell::Cell,
    io,
    ops::{Deref, DerefMut},
    pin::Pin,
    process::Stdio,
    rc::Rc,
};
use task::{Shell, ShellBuilder};
use util::{ResultExt as _, process::Child};

pub(super) struct StdioProcess {
    pub child: AcpProcess,
    pub incoming: BoxStream<'static, io::Result<String>>,
    pub outgoing: Pin<Box<dyn Sink<String, Error = io::Error> + Send>>,
    // The connection owns when this starts and when it is cancelled.
    pub stderr: BoxFuture<'static, Result<()>>,
    pub debug_log: AcpDebugLog,
}

#[cfg(test)]
pub(super) fn spawn_stdio(
    project: &Entity<Project>,
    command: AgentServerCommand,
    cx: &mut AsyncApp,
) -> Result<StdioProcess> {
    spawn_stdio_with_options(project, command, &super::AcpLaunchOptions::default(), cx)
}

pub(super) struct AcpProcess {
    child: Child,
    exited: Option<Rc<Cell<bool>>>,
    _quit_subscription: Option<Subscription>,
}

impl AcpProcess {
    fn new(child: Child, guarded: bool, cx: &mut AsyncApp) -> Self {
        let exited = guarded.then(|| Rc::new(Cell::new(false)));
        let mut process = Self {
            child,
            exited,
            _quit_subscription: None,
        };
        #[cfg(unix)]
        if let Some(exited) = process.exited.clone() {
            let pid = process.child.id();
            process._quit_subscription = Some(cx.update(|cx| {
                cx.on_app_quit(move |_| {
                    if !exited.get() {
                        match kill_process_group(pid) {
                            Ok(()) => exited.set(true),
                            Err(error) => {
                                log::error!("Failed to terminate ACP process group {pid}: {error}")
                            }
                        }
                    }
                    futures::future::ready(())
                })
            }));
        }
        process
    }

    pub fn exit_flag(&self) -> Option<Rc<Cell<bool>>> {
        self.exited.clone()
    }

    pub fn terminate(&mut self) -> Result<()> {
        if self.exited.as_ref().is_some_and(|exited| exited.get()) {
            return Ok(());
        }
        #[cfg(unix)]
        if self.exited.is_some() {
            kill_process_group(self.child.id())?;
        } else {
            self.child.kill()?;
        }
        #[cfg(not(unix))]
        self.child.kill()?;
        if let Some(exited) = &self.exited {
            exited.set(true);
        }
        Ok(())
    }
}

impl Deref for AcpProcess {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.child
    }
}

impl DerefMut for AcpProcess {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}

impl Drop for AcpProcess {
    fn drop(&mut self) {
        if self.exited.is_some() {
            self.terminate().log_err();
        }
    }
}

#[cfg(unix)]
fn kill_process_group(pid: u32) -> Result<()> {
    if unsafe { libc::killpg(pid as i32, libc::SIGKILL) } == -1 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error.into());
        }
    }
    Ok(())
}

pub(super) fn spawn_stdio_with_options(
    project: &Entity<Project>,
    command: AgentServerCommand,
    options: &super::AcpLaunchOptions,
    cx: &mut AsyncApp,
) -> Result<StdioProcess> {
    let root_dir = project.read_with(cx, |project, cx| {
        project
            .default_path_list(cx)
            .ordered_paths()
            .next()
            .cloned()
    });
    let (path, arguments, environment) = project
        .read_with(cx, |project, cx| {
            project.remote_client().and_then(|client| {
                let template = client
                    .read(cx)
                    .build_command(
                        Some(command.path.display().to_string()),
                        &command.args,
                        &command.env.clone().into_iter().flatten().collect(),
                        root_dir.as_ref().map(|path| path.display().to_string()),
                        None,
                        Interactive::No,
                    )
                    .log_err()?;
                Some((template.program, template.args, template.env))
            })
        })
        .unwrap_or_else(|| {
            (
                command.path.display().to_string(),
                command.args,
                command.env.unwrap_or_default(),
            )
        });

    let builder = ShellBuilder::new(&Shell::System, cfg!(windows)).non_interactive();
    let mut child = builder.build_std_command(Some(path.clone()), &arguments);
    child.envs(environment);
    if let Some(cwd) = project.read_with(cx, |project, _cx| {
        if project.is_local() {
            options.working_directory.as_ref().or(root_dir.as_ref())
        } else {
            None
        }
    }) {
        child.current_dir(cwd);
    }
    let child = Child::spawn(child, Stdio::piped(), Stdio::piped(), Stdio::piped())?;
    let mut child = AcpProcess::new(child, options.terminate_on_drop_and_quit, cx);

    let stdout = child.stdout.take().context("Failed to take stdout")?;
    let stdin = child.stdin.take().context("Failed to take stdin")?;
    let stderr = child.stderr.take().context("Failed to take stderr")?;
    log::debug!(
        "Spawning external agent server: {:?}, {:?}",
        path,
        arguments
    );
    log::trace!("Spawned (pid: {})", child.id());

    let debug_log = AcpDebugLog::default();
    let incoming = BufReader::new(stdout)
        .lines()
        .inspect({
            let debug_log = debug_log.clone();
            move |result| match result {
                Ok(line) => debug_log.record_line(AcpDebugMessageDirection::Incoming, line),
                Err(error) => {
                    log::warn!("ACP transport read error: {error}");
                }
            }
        })
        .boxed();

    let outgoing = Box::pin(futures::sink::unfold(
        (Box::pin(stdin), debug_log.clone()),
        async move |(mut writer, debug_log), line: String| {
            debug_log.record_line(AcpDebugMessageDirection::Outgoing, &line);
            let mut bytes = line.into_bytes();
            bytes.push(b'\n');
            writer.write_all(&bytes).await?;
            Ok::<_, io::Error>((writer, debug_log))
        },
    ));

    let stderr = {
        let debug_log = debug_log.clone();
        async move {
            let mut stderr = BufReader::new(stderr);
            let mut line = String::new();
            while let Ok(bytes_read) = stderr.read_line(&mut line).await
                && bytes_read > 0
            {
                let trimmed = line.trim_end_matches(['\n', '\r']);
                log::warn!("agent stderr: {trimmed}");
                debug_log.record_line(AcpDebugMessageDirection::Stderr, trimmed);
                line.clear();
            }
            Ok(())
        }
        .boxed()
    };

    Ok(StdioProcess {
        child,
        incoming,
        outgoing,
        stderr,
        debug_log,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::acp::AcpDebugMessageContent;
    use collections::HashMap;
    use futures::SinkExt as _;
    use gpui::TestAppContext;

    async fn guarded_process(cx: &mut TestAppContext) -> (StdioProcess, tempfile::TempDir) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
        });
        cx.executor().allow_parking();
        let directory = tempfile::tempdir().expect("temporary directory");
        let project = Project::example([directory.path()], &mut cx.to_async()).await;
        let options = crate::AcpLaunchOptions {
            working_directory: Some(directory.path().to_owned()),
            terminate_on_drop_and_quit: true,
            ..Default::default()
        };
        let command = AgentServerCommand {
            path: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "/bin/sleep 30 & printf 'ready\\n'; IFS= read -r line; wait".into(),
            ],
            env: None,
        };
        let process = spawn_stdio_with_options(&project, command, &options, &mut cx.to_async())
            .expect("spawn guarded process");
        (process, directory)
    }

    #[gpui::test]
    async fn guarded_drop_terminates_the_process_group(cx: &mut TestAppContext) {
        let (mut process, _directory) = guarded_process(cx).await;
        assert_eq!(
            process
                .incoming
                .next()
                .await
                .expect("ready line")
                .expect("read stdout"),
            "ready"
        );
        assert!(
            process
                .child
                .try_status()
                .expect("check running parent")
                .is_none()
        );
        let status = process.child.status();
        let mut incoming = process.incoming;
        drop(process.child);
        assert!(!status.await.expect("wait for killed parent").success());
        assert!(
            incoming.next().await.is_none(),
            "grandchild must close inherited stdout"
        );
    }

    #[gpui::test]
    async fn guarded_quit_terminates_even_with_a_live_owner(cx: &mut TestAppContext) {
        let (mut process, _directory) = guarded_process(cx).await;
        assert_eq!(
            process
                .incoming
                .next()
                .await
                .expect("ready line")
                .expect("read stdout"),
            "ready"
        );
        assert!(
            process
                .child
                .try_status()
                .expect("check running parent")
                .is_none()
        );
        let status = process.child.status();
        cx.update(|cx| cx.shutdown());
        assert!(!status.await.expect("wait for quit termination").success());
        assert!(process.child.exit_flag().expect("guarded flag").get());
        assert!(process.incoming.next().await.is_none());
    }

    #[gpui::test]
    async fn launch_options_override_the_project_directory(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
        });
        cx.executor().allow_parking();
        let first = tempfile::tempdir().expect("first Arena");
        let second = tempfile::tempdir().expect("second Arena");
        let project = Project::example([first.path()], &mut cx.to_async()).await;
        for directory in [first.path(), second.path()] {
            let options = crate::AcpLaunchOptions {
                working_directory: Some(directory.to_owned()),
                terminate_on_drop_and_quit: true,
                ..Default::default()
            };
            let command = AgentServerCommand {
                path: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    "pwd -P; printf '%s\\n' \"$ACP_ARENA_TEST\"".into(),
                ],
                env: Some(HashMap::from_iter([(
                    "ACP_ARENA_TEST".into(),
                    directory.display().to_string(),
                )])),
            };
            let StdioProcess {
                mut child,
                mut incoming,
                ..
            } = spawn_stdio_with_options(&project, command, &options, &mut cx.to_async())
                .expect("spawn in Arena");
            assert_eq!(
                incoming.next().await.expect("cwd line").expect("read cwd"),
                directory
                    .canonicalize()
                    .expect("canonical directory")
                    .display()
                    .to_string()
            );
            assert_eq!(
                incoming.next().await.expect("env line").expect("read env"),
                directory.display().to_string()
            );
            assert!(child.status().await.expect("successful process").success());
            child.exit_flag().expect("guarded flag").set(true);
        }
    }

    #[gpui::test]
    async fn missing_bare_command_reports_exit_127(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
        });
        cx.executor().allow_parking();
        let directory = tempfile::tempdir().expect("Arena directory");
        let project = Project::example([directory.path()], &mut cx.to_async()).await;
        let command = AgentServerCommand {
            path: "agentium-nonexistent-codex-acp-test".into(),
            args: Vec::new(),
            env: None,
        };
        let options = crate::AcpLaunchOptions {
            working_directory: Some(directory.path().to_owned()),
            terminate_on_drop_and_quit: true,
            ..Default::default()
        };
        let mut process = spawn_stdio_with_options(&project, command, &options, &mut cx.to_async())
            .expect("shell spawn succeeds even when adapter is missing");
        let status = process
            .child
            .status()
            .await
            .expect("wait for missing command");
        process.child.exit_flag().expect("guarded flag").set(true);
        assert_eq!(status.code(), Some(127));
        process.stderr.await.expect("read missing-command stderr");
        let (messages, _receiver) = process.debug_log.subscribe();
        assert!(
            messages
                .iter()
                .any(|message| message.direction == AcpDebugMessageDirection::Stderr)
        );
    }

    #[gpui::test]
    async fn repeated_spawns_preserve_command_and_stdio(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        cx.executor().allow_parking();
        let directory = tempfile::tempdir().expect("create working directory");
        let expected_directory = directory
            .path()
            .canonicalize()
            .expect("resolve working directory");
        let project = Project::example([directory.path()], &mut cx.to_async()).await;
        let command = AgentServerCommand {
            path: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                r#"printf '%s\n' "$ACP_TRANSPORT_TEST"; pwd -P; printf 'transport stderr\r\n' >&2; IFS= read -r line && printf '%s\n' "$line""#.into(),
            ],
            env: Some(HashMap::from_iter([(
                "ACP_TRANSPORT_TEST".into(),
                "value with spaces and $literal".into(),
            )])),
        };

        for attempt in 0..2 {
            let StdioProcess {
                mut child,
                mut incoming,
                mut outgoing,
                stderr,
                debug_log,
            } = spawn_stdio(&project, command.clone(), &mut cx.to_async())
                .expect("spawn fresh transport");
            let stderr_task = cx.background_executor.spawn(stderr);
            let params = serde_json::json!({ "attempt": attempt });
            let request = serde_json::json!({
                "jsonrpc": "2.0",
                "id": attempt,
                "method": "transport/test",
                "params": params,
            })
            .to_string();
            let exchange = async {
                outgoing.send(request.clone()).await.expect("write request");
                drop(outgoing);

                let mut output = Vec::new();
                while let Some(line) = incoming.next().await {
                    output.push(line.expect("read output"));
                }
                assert_eq!(
                    output,
                    [
                        "value with spaces and $literal".to_string(),
                        expected_directory.to_string_lossy().into_owned(),
                        request,
                    ]
                );
                assert!(child.status().await.expect("wait for process").success());
                stderr_task.await.expect("read stderr");
            }
            .fuse();
            let timeout = cx
                .background_executor
                .timer(std::time::Duration::from_secs(5))
                .fuse();
            futures::pin_mut!(exchange, timeout);
            futures::select! {
                _ = exchange => {}
                _ = timeout => panic!("timed out waiting for stdio exchange on attempt {attempt}"),
            }

            let (messages, _receiver) = debug_log.subscribe();
            assert_eq!(messages.len(), 3, "each process must have its own log");
            for direction in [
                AcpDebugMessageDirection::Outgoing,
                AcpDebugMessageDirection::Incoming,
            ] {
                assert!(messages.iter().any(|message| {
                    message.direction == direction
                        && matches!(
                            &message.message,
                            AcpDebugMessageContent::Request { method, params: Some(recorded), .. }
                                if method.as_ref() == "transport/test" && recorded == &params
                        )
                }));
            }
            assert!(messages.iter().any(|message| {
                message.direction == AcpDebugMessageDirection::Stderr
                    && matches!(
                        &message.message,
                        AcpDebugMessageContent::Stderr { line } if line.as_ref() == "transport stderr"
                    )
            }));
        }
    }
}
