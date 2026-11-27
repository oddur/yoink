//! `FakeDockerOps`: scripted typed responses and a call log, so the
//! rest of the crate is testable without a daemon.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use bollard::models::ContainerCreateBody;
use tokio::sync::mpsc;

use super::{
    ContainerDetail, ContainerInfo, ContainerStats, DockerError, DockerEvent, DockerOps,
    DockerVersion, ExecSession, HostInfo, ImageTarStream, LogLine, NetworkInfo, OneShotResult,
    StoppedRemoval, VolumeInfo,
};
use crate::base::host::Host;

// ───── FakeDockerOps ────────────────────────────────────────────────────

/// Test double. Push scripted typed responses keyed by op kind, then assert
/// against `calls()` afterwards.
#[derive(Default)]
pub struct FakeDockerOps {
    state: Mutex<FakeState>,
}

#[derive(Default)]
struct FakeState {
    version: VecDeque<Result<DockerVersion, DockerError>>,
    host_info: VecDeque<Result<HostInfo, DockerError>>,
    container_stats: VecDeque<Result<ContainerStats, DockerError>>,
    ensure_network: VecDeque<Result<bool, DockerError>>,
    pull_image: VecDeque<Result<(), DockerError>>,
    /// `(image, serveraddress of the credentials)` for each pull, in
    /// call order. `None` is an anonymous pull.
    pull_auth: Vec<(String, Option<String>)>,
    /// Host address → how long each `pull_image` on that host takes.
    /// For tests that need one slow host.
    pull_delays: HashMap<String, Duration>,
    load_image: VecDeque<Result<(), DockerError>>,
    build_image: VecDeque<Result<(), DockerError>>,
    /// `image:tag` → presence flag. Defaults `false` so existing tests
    /// (which expect pulls to actually fire) keep working. After a
    /// successful `build_image` the entry is upserted to `true`.
    images_present: HashMap<String, bool>,
    list_containers: VecDeque<Result<Vec<ContainerInfo>, DockerError>>,
    create_container: VecDeque<Result<String, DockerError>>,
    start_container: VecDeque<Result<(), DockerError>>,
    stop_container: VecDeque<Result<(), DockerError>>,
    force_remove_container: VecDeque<Result<(), DockerError>>,
    remove_stopped_container: VecDeque<Result<StoppedRemoval, DockerError>>,
    healthcheck: VecDeque<Result<u16, DockerError>>,
    healthcheck_tcp: VecDeque<Result<(), DockerError>>,
    exec_oneshot: VecDeque<Result<OneShotResult, DockerError>>,
    exec_with_stdin: VecDeque<Result<OneShotResult, DockerError>>,
    /// How long `exec_with_stdin` waits before it answers. Lets a test
    /// with paused time drive a caller timeout.
    exec_with_stdin_delay: Option<std::time::Duration>,
    curl_in_netns: VecDeque<Result<OneShotResult, DockerError>>,
    fetch_logs: VecDeque<Result<Vec<String>, DockerError>>,
    one_shot: VecDeque<Result<OneShotResult, DockerError>>,
    event_streams: VecDeque<Vec<DockerEvent>>,
    log_streams: VecDeque<Vec<LogLine>>,
    calls: Vec<RecordedCall>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedCall {
    Version(Host),
    HostInfo(Host),
    ContainerStats(Host, String),
    EnsureNetwork(Host, String),
    PullImage(Host, String, String),
    LoadImage(Host),
    BuildImage(Host, String, usize),
    ListContainersByLabel(Host, String),
    ListRunningContainers(Host),
    CreateContainer(Host, String),
    StartContainer(Host, String),
    StopContainer(Host, String, Duration),
    ForceRemoveContainer(Host, String),
    RemoveStoppedContainer(Host, String),
    Healthcheck(Host, String, String, u16, String),
    HealthcheckTcp(Host, String, String, u16),
    ExecOneshot(Host, String, Vec<String>),
    ExecWithStdin(Host, String, Vec<String>, Vec<u8>),
    CurlInNetns(Host, String, Vec<String>),
    FetchRecentLogs(Host, String, u32),
    OpenLogStream(Host, String),
    RunOneShot(Host, String),
    SubscribeEvents(Host),
}

impl FakeDockerOps {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state.lock().expect("FakeDockerOps mutex poisoned")
    }

    pub fn push_version(&self, v: Result<DockerVersion, DockerError>) {
        self.lock().version.push_back(v);
    }
    pub fn push_host_info(&self, v: Result<HostInfo, DockerError>) {
        self.lock().host_info.push_back(v);
    }
    pub fn push_container_stats(&self, v: Result<ContainerStats, DockerError>) {
        self.lock().container_stats.push_back(v);
    }
    pub fn push_ensure_network(&self, v: Result<bool, DockerError>) {
        self.lock().ensure_network.push_back(v);
    }
    pub fn push_pull_image(&self, v: Result<(), DockerError>) {
        self.lock().pull_image.push_back(v);
    }
    /// Make every `pull_image` on `host_address` take `delay` (tokio
    /// time, so a paused-time test can step through it).
    pub fn delay_pulls_on(&self, host_address: &str, delay: Duration) {
        self.lock().pull_delays.insert(host_address.into(), delay);
    }
    pub fn push_build_image(&self, v: Result<(), DockerError>) {
        self.lock().build_image.push_back(v);
    }
    /// Mark `image:tag` as already present so `image_present` returns
    /// `true` and the caller skips its pull/build.
    pub fn mark_image_present(&self, image: &str, tag: &str) {
        let key = crate::base::image::image_reference(image, tag);
        self.lock().images_present.insert(key, true);
    }
    pub fn push_list_containers(&self, v: Result<Vec<ContainerInfo>, DockerError>) {
        self.lock().list_containers.push_back(v);
    }
    pub fn push_create_container(&self, v: Result<String, DockerError>) {
        self.lock().create_container.push_back(v);
    }
    pub fn push_start_container(&self, v: Result<(), DockerError>) {
        self.lock().start_container.push_back(v);
    }
    pub fn push_stop_container(&self, v: Result<(), DockerError>) {
        self.lock().stop_container.push_back(v);
    }
    pub fn push_force_remove(&self, v: Result<(), DockerError>) {
        self.lock().force_remove_container.push_back(v);
    }
    pub fn push_remove_stopped(&self, v: Result<StoppedRemoval, DockerError>) {
        self.lock().remove_stopped_container.push_back(v);
    }
    pub fn push_healthcheck(&self, v: Result<u16, DockerError>) {
        self.lock().healthcheck.push_back(v);
    }
    pub fn push_healthcheck_tcp(&self, v: Result<(), DockerError>) {
        self.lock().healthcheck_tcp.push_back(v);
    }
    pub fn push_exec_oneshot(&self, v: Result<OneShotResult, DockerError>) {
        self.lock().exec_oneshot.push_back(v);
    }
    pub fn push_exec_with_stdin(&self, v: Result<OneShotResult, DockerError>) {
        self.lock().exec_with_stdin.push_back(v);
    }
    pub fn set_exec_with_stdin_delay(&self, d: std::time::Duration) {
        self.lock().exec_with_stdin_delay = Some(d);
    }
    pub fn push_curl_in_netns(&self, v: Result<OneShotResult, DockerError>) {
        self.lock().curl_in_netns.push_back(v);
    }
    pub fn push_fetch_logs(&self, v: Result<Vec<String>, DockerError>) {
        self.lock().fetch_logs.push_back(v);
    }
    pub fn push_log_stream(&self, lines: Vec<LogLine>) {
        self.lock().log_streams.push_back(lines);
    }
    pub fn push_one_shot(&self, v: Result<OneShotResult, DockerError>) {
        self.lock().one_shot.push_back(v);
    }
    pub fn push_event_stream(&self, events: Vec<DockerEvent>) {
        self.lock().event_streams.push_back(events);
    }

    #[must_use]
    /// `(image, registry the credentials were for)` for each pull.
    pub fn pull_auth(&self) -> Vec<(String, Option<String>)> {
        self.lock().pull_auth.clone()
    }

    pub fn calls(&self) -> Vec<RecordedCall> {
        self.lock().calls.clone()
    }
}

fn pop<T>(q: &mut VecDeque<Result<T, DockerError>>, name: &'static str) -> Result<T, DockerError> {
    q.pop_front()
        .unwrap_or(Err(DockerError::FakeExhausted(name)))
}

#[async_trait]
impl DockerOps for FakeDockerOps {
    async fn version(&self, host: &Host) -> Result<DockerVersion, DockerError> {
        let mut s = self.lock();
        s.calls.push(RecordedCall::Version(host.clone()));
        pop(&mut s.version, "version")
    }
    async fn host_info(&self, host: &Host) -> Result<HostInfo, DockerError> {
        let mut s = self.lock();
        s.calls.push(RecordedCall::HostInfo(host.clone()));
        pop(&mut s.host_info, "host_info")
    }
    async fn container_stats(
        &self,
        host: &Host,
        name: &str,
    ) -> Result<ContainerStats, DockerError> {
        let mut s = self.lock();
        s.calls
            .push(RecordedCall::ContainerStats(host.clone(), name.into()));
        pop(&mut s.container_stats, "container_stats")
    }
    async fn inspect_container(
        &self,
        _host: &Host,
        name: &str,
    ) -> Result<ContainerDetail, DockerError> {
        Ok(ContainerDetail {
            name: name.into(),
            ..Default::default()
        })
    }
    async fn list_networks(&self, _host: &Host) -> Result<Vec<NetworkInfo>, DockerError> {
        Ok(Vec::new())
    }
    async fn connect_container_network(
        &self,
        _host: &Host,
        _container: &str,
        _network: &str,
        _aliases: &[String],
    ) -> Result<(), DockerError> {
        Ok(())
    }
    async fn list_volumes(&self, _host: &Host) -> Result<Vec<VolumeInfo>, DockerError> {
        Ok(Vec::new())
    }
    async fn ensure_network(&self, host: &Host, network: &str) -> Result<bool, DockerError> {
        let mut s = self.lock();
        s.calls
            .push(RecordedCall::EnsureNetwork(host.clone(), network.into()));
        pop(&mut s.ensure_network, "ensure_network")
    }
    async fn pull_image(
        &self,
        host: &Host,
        image: &str,
        tag: &str,
        credentials: Option<bollard::auth::DockerCredentials>,
    ) -> Result<(), DockerError> {
        let delay = {
            let mut s = self.lock();
            s.pull_auth
                .push((image.into(), credentials.and_then(|c| c.serveraddress)));
            s.calls.push(RecordedCall::PullImage(
                host.clone(),
                image.into(),
                tag.into(),
            ));
            s.pull_delays.get(&host.address).copied()
        };
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        pop(&mut self.lock().pull_image, "pull_image")
    }
    async fn image_present(
        &self,
        _host: &Host,
        image: &str,
        tag: &str,
    ) -> Result<bool, DockerError> {
        let key = crate::base::image::image_reference(image, tag);
        Ok(*self.lock().images_present.get(&key).unwrap_or(&false))
    }
    async fn load_image(&self, host: &Host, _body: ImageTarStream) -> Result<(), DockerError> {
        let mut s = self.lock();
        s.calls.push(RecordedCall::LoadImage(host.clone()));
        pop(&mut s.load_image, "load_image")
    }
    async fn build_image(
        &self,
        host: &Host,
        tag: &str,
        context_tar: bytes::Bytes,
    ) -> Result<(), DockerError> {
        let mut s = self.lock();
        s.calls.push(RecordedCall::BuildImage(
            host.clone(),
            tag.into(),
            context_tar.len(),
        ));
        let result = pop(&mut s.build_image, "build_image");
        if result.is_ok() {
            // Successful build leaves the image present for subsequent
            // `image_present` checks — mirrors real docker behavior.
            s.images_present.insert(tag.into(), true);
        }
        result
    }
    async fn list_containers_by_label(
        &self,
        host: &Host,
        label: &str,
    ) -> Result<Vec<ContainerInfo>, DockerError> {
        let mut s = self.lock();
        s.calls.push(RecordedCall::ListContainersByLabel(
            host.clone(),
            label.into(),
        ));
        pop(&mut s.list_containers, "list_containers_by_label")
    }
    async fn list_running_containers(
        &self,
        host: &Host,
    ) -> Result<Vec<ContainerInfo>, DockerError> {
        let mut s = self.lock();
        s.calls
            .push(RecordedCall::ListRunningContainers(host.clone()));
        pop(&mut s.list_containers, "list_running_containers")
    }
    async fn create_container(
        &self,
        host: &Host,
        name: &str,
        _body: ContainerCreateBody,
    ) -> Result<String, DockerError> {
        let mut s = self.lock();
        s.calls
            .push(RecordedCall::CreateContainer(host.clone(), name.into()));
        pop(&mut s.create_container, "create_container")
    }
    async fn start_container(&self, host: &Host, name: &str) -> Result<(), DockerError> {
        let mut s = self.lock();
        s.calls
            .push(RecordedCall::StartContainer(host.clone(), name.into()));
        pop(&mut s.start_container, "start_container")
    }
    async fn stop_container(
        &self,
        host: &Host,
        name: &str,
        drain: Duration,
    ) -> Result<(), DockerError> {
        let mut s = self.lock();
        s.calls.push(RecordedCall::StopContainer(
            host.clone(),
            name.into(),
            drain,
        ));
        pop(&mut s.stop_container, "stop_container")
    }
    async fn force_remove_container(&self, host: &Host, name: &str) -> Result<(), DockerError> {
        let mut s = self.lock();
        s.calls.push(RecordedCall::ForceRemoveContainer(
            host.clone(),
            name.into(),
        ));
        pop(&mut s.force_remove_container, "force_remove_container")
    }
    async fn remove_stopped_container(
        &self,
        host: &Host,
        name: &str,
    ) -> Result<StoppedRemoval, DockerError> {
        let mut s = self.lock();
        s.calls.push(RecordedCall::RemoveStoppedContainer(
            host.clone(),
            name.into(),
        ));
        pop(&mut s.remove_stopped_container, "remove_stopped_container")
    }
    async fn kill_container(&self, _host: &Host, _name: &str) -> Result<(), DockerError> {
        Ok(())
    }
    async fn healthcheck(
        &self,
        host: &Host,
        network: &str,
        target: &str,
        port: u16,
        path: &str,
    ) -> Result<u16, DockerError> {
        let mut s = self.lock();
        s.calls.push(RecordedCall::Healthcheck(
            host.clone(),
            network.into(),
            target.into(),
            port,
            path.into(),
        ));
        pop(&mut s.healthcheck, "healthcheck")
    }
    async fn healthcheck_tcp(
        &self,
        host: &Host,
        network: &str,
        target: &str,
        port: u16,
    ) -> Result<(), DockerError> {
        let mut s = self.lock();
        s.calls.push(RecordedCall::HealthcheckTcp(
            host.clone(),
            network.into(),
            target.into(),
            port,
        ));
        pop(&mut s.healthcheck_tcp, "healthcheck_tcp")
    }
    async fn exec_oneshot(
        &self,
        host: &Host,
        container: &str,
        cmd: Vec<String>,
    ) -> Result<OneShotResult, DockerError> {
        let mut s = self.lock();
        s.calls.push(RecordedCall::ExecOneshot(
            host.clone(),
            container.into(),
            cmd,
        ));
        pop(&mut s.exec_oneshot, "exec_oneshot")
    }
    async fn curl_in_netns(
        &self,
        host: &Host,
        container: &str,
        args: Vec<String>,
    ) -> Result<OneShotResult, DockerError> {
        let mut s = self.lock();
        s.calls.push(RecordedCall::CurlInNetns(
            host.clone(),
            container.into(),
            args,
        ));
        pop(&mut s.curl_in_netns, "curl_in_netns")
    }
    async fn exec_with_stdin(
        &self,
        host: &Host,
        container: &str,
        cmd: Vec<String>,
        stdin: Vec<u8>,
    ) -> Result<OneShotResult, DockerError> {
        let (result, delay) = {
            let mut s = self.lock();
            s.calls.push(RecordedCall::ExecWithStdin(
                host.clone(),
                container.into(),
                cmd,
                stdin,
            ));
            (
                pop(&mut s.exec_with_stdin, "exec_with_stdin"),
                s.exec_with_stdin_delay,
            )
        };
        if let Some(d) = delay {
            tokio::time::sleep(d).await;
        }
        result
    }
    async fn exec_interactive(
        &self,
        _host: &Host,
        _container: &str,
        _cmd: Vec<String>,
        _rows: u16,
        _cols: u16,
    ) -> Result<ExecSession, DockerError> {
        Err(DockerError::Invalid(
            "exec_interactive not supported by FakeDockerOps".into(),
        ))
    }
    async fn resize_exec(
        &self,
        _host: &Host,
        _exec_id: &str,
        _rows: u16,
        _cols: u16,
    ) -> Result<(), DockerError> {
        Ok(())
    }
    async fn resize_container_tty(
        &self,
        _host: &Host,
        _container: &str,
        _rows: u16,
        _cols: u16,
    ) -> Result<(), DockerError> {
        Ok(())
    }
    async fn start_debug_sidecar(
        &self,
        _host: &Host,
        _target_container: &str,
        _image: &str,
        _rows: u16,
        _cols: u16,
    ) -> Result<ExecSession, DockerError> {
        Err(DockerError::Invalid(
            "start_debug_sidecar not supported by FakeDockerOps".into(),
        ))
    }
    async fn fetch_recent_logs(
        &self,
        host: &Host,
        name: &str,
        lines: u32,
    ) -> Result<Vec<String>, DockerError> {
        let mut s = self.lock();
        s.calls.push(RecordedCall::FetchRecentLogs(
            host.clone(),
            name.into(),
            lines,
        ));
        pop(&mut s.fetch_logs, "fetch_recent_logs")
    }
    async fn open_log_stream(
        &self,
        host: &Host,
        name: &str,
        _tail_lines: u32,
    ) -> Result<mpsc::UnboundedReceiver<LogLine>, DockerError> {
        let mut s = self.lock();
        s.calls
            .push(RecordedCall::OpenLogStream(host.clone(), name.into()));
        let (tx, rx) = mpsc::unbounded_channel();
        if let Some(lines) = s.log_streams.pop_front() {
            for line in lines {
                let _ = tx.send(line);
            }
        }
        Ok(rx)
    }
    async fn run_one_shot(
        &self,
        host: &Host,
        name: &str,
        _body: ContainerCreateBody,
    ) -> Result<OneShotResult, DockerError> {
        let mut s = self.lock();
        s.calls
            .push(RecordedCall::RunOneShot(host.clone(), name.into()));
        pop(&mut s.one_shot, "run_one_shot")
    }
    async fn subscribe_events(
        &self,
        host: &Host,
    ) -> Result<mpsc::UnboundedReceiver<DockerEvent>, DockerError> {
        let mut s = self.lock();
        s.calls.push(RecordedCall::SubscribeEvents(host.clone()));
        let (tx, rx) = mpsc::unbounded_channel();
        if let Some(events) = s.event_streams.pop_front() {
            for e in events {
                let _ = tx.send(e);
            }
        }
        Ok(rx)
    }
}
