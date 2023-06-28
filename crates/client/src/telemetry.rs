use crate::{TelemetrySettings, ZED_SECRET_CLIENT_TOKEN, ZED_SERVER_URL};
use gpui::{executor::Background, serde_json, AppContext, Task};
use lazy_static::lazy_static;
use parking_lot::Mutex;
use serde::Serialize;
use std::{env, io::Write, mem, path::PathBuf, sync::Arc, time::Duration};
use tempfile::NamedTempFile;
use util::http::HttpClient;
use util::{channel::ReleaseChannel, TryFutureExt};

pub struct Telemetry {
    http_client: Arc<dyn HttpClient>,
    executor: Arc<Background>,
    state: Mutex<TelemetryState>,
}

#[derive(Default)]
struct TelemetryState {
    metrics_id: Option<Arc<str>>,      // Per logged-in user
    installation_id: Option<Arc<str>>, // Per app installation
    app_version: Option<Arc<str>>,
    release_channel: Option<&'static str>,
    os_name: &'static str,
    os_version: Option<Arc<str>>,
    architecture: &'static str,
    clickhouse_events_queue: Vec<ClickhouseEventWrapper>,
    flush_clickhouse_events_task: Option<Task<()>>,
    log_file: Option<NamedTempFile>,
    is_staff: Option<bool>,
}

const CLICKHOUSE_EVENTS_URL_PATH: &'static str = "/api/events";

lazy_static! {
    static ref CLICKHOUSE_EVENTS_URL: String =
        format!("{}{}", *ZED_SERVER_URL, CLICKHOUSE_EVENTS_URL_PATH);
}

#[derive(Serialize, Debug)]
struct ClickhouseEventRequestBody {
    token: &'static str,
    installation_id: Option<Arc<str>>,
    app_version: Option<Arc<str>>,
    os_name: &'static str,
    os_version: Option<Arc<str>>,
    architecture: &'static str,
    release_channel: Option<&'static str>,
    events: Vec<ClickhouseEventWrapper>,
}

#[derive(Serialize, Debug)]
struct ClickhouseEventWrapper {
    signed_in: bool,
    #[serde(flatten)]
    event: ClickhouseEvent,
}

#[derive(Serialize, Debug)]
#[serde(tag = "type")]
pub enum ClickhouseEvent {
    Editor {
        operation: &'static str,
        file_extension: Option<String>,
        vim_mode: bool,
        copilot_enabled: bool,
        copilot_enabled_for_language: bool,
    },
    Copilot {
        suggestion_id: Option<String>,
        suggestion_accepted: bool,
        file_extension: Option<String>,
    },
}

#[cfg(debug_assertions)]
const MAX_QUEUE_LEN: usize = 1;

#[cfg(not(debug_assertions))]
const MAX_QUEUE_LEN: usize = 10;

#[cfg(debug_assertions)]
const DEBOUNCE_INTERVAL: Duration = Duration::from_secs(1);

#[cfg(not(debug_assertions))]
const DEBOUNCE_INTERVAL: Duration = Duration::from_secs(30);

impl Telemetry {
    pub fn new(client: Arc<dyn HttpClient>, cx: &AppContext) -> Arc<Self> {
        let platform = cx.platform();
        let release_channel = if cx.has_global::<ReleaseChannel>() {
            Some(cx.global::<ReleaseChannel>().display_name())
        } else {
            None
        };
        // TODO: Replace all hardware stuff with nested SystemSpecs json
        let this = Arc::new(Self {
            http_client: client,
            executor: cx.background().clone(),
            state: Mutex::new(TelemetryState {
                os_name: platform.os_name().into(),
                os_version: platform.os_version().ok().map(|v| v.to_string().into()),
                architecture: env::consts::ARCH,
                app_version: platform.app_version().ok().map(|v| v.to_string().into()),
                release_channel,
                installation_id: None,
                metrics_id: None,
                clickhouse_events_queue: Default::default(),
                flush_clickhouse_events_task: Default::default(),
                log_file: None,
                is_staff: None,
            }),
        });

        this
    }

    pub fn log_file_path(&self) -> Option<PathBuf> {
        Some(self.state.lock().log_file.as_ref()?.path().to_path_buf())
    }

    pub fn start(self: &Arc<Self>, installation_id: Option<String>) {
        let mut state = self.state.lock();
        state.installation_id = installation_id.map(|id| id.into());
        let has_clickhouse_events = !state.clickhouse_events_queue.is_empty();
        drop(state);

        if has_clickhouse_events {
            self.flush_clickhouse_events();
        }
    }

    /// This method takes the entire TelemetrySettings struct in order to force client code
    /// to pull the struct out of the settings global. Do not remove!
    pub fn set_authenticated_user_info(
        self: &Arc<Self>,
        metrics_id: Option<String>,
        is_staff: bool,
        cx: &AppContext,
    ) {
        if !settings::get::<TelemetrySettings>(cx).metrics {
            return;
        }

        let mut state = self.state.lock();
        let metrics_id: Option<Arc<str>> = metrics_id.map(|id| id.into());
        state.metrics_id = metrics_id.clone();
        state.is_staff = Some(is_staff);
        drop(state);
    }

    pub fn report_clickhouse_event(
        self: &Arc<Self>,
        event: ClickhouseEvent,
        telemetry_settings: TelemetrySettings,
    ) {
        if !telemetry_settings.metrics {
            return;
        }

        let mut state = self.state.lock();
        let signed_in = state.metrics_id.is_some();
        state
            .clickhouse_events_queue
            .push(ClickhouseEventWrapper { signed_in, event });

        if state.installation_id.is_some() {
            if state.clickhouse_events_queue.len() >= MAX_QUEUE_LEN {
                drop(state);
                self.flush_clickhouse_events();
            } else {
                let this = self.clone();
                let executor = self.executor.clone();
                state.flush_clickhouse_events_task = Some(self.executor.spawn(async move {
                    executor.timer(DEBOUNCE_INTERVAL).await;
                    this.flush_clickhouse_events();
                }));
            }
        }
    }

    pub fn metrics_id(self: &Arc<Self>) -> Option<Arc<str>> {
        self.state.lock().metrics_id.clone()
    }

    pub fn installation_id(self: &Arc<Self>) -> Option<Arc<str>> {
        self.state.lock().installation_id.clone()
    }

    pub fn is_staff(self: &Arc<Self>) -> Option<bool> {
        self.state.lock().is_staff
    }

    fn flush_clickhouse_events(self: &Arc<Self>) {
        let mut state = self.state.lock();
        let mut events = mem::take(&mut state.clickhouse_events_queue);
        state.flush_clickhouse_events_task.take();
        drop(state);

        let this = self.clone();
        self.executor
            .spawn(
                async move {
                    let mut json_bytes = Vec::new();

                    if let Some(file) = &mut this.state.lock().log_file {
                        let file = file.as_file_mut();
                        for event in &mut events {
                            json_bytes.clear();
                            serde_json::to_writer(&mut json_bytes, event)?;
                            file.write_all(&json_bytes)?;
                            file.write(b"\n")?;
                        }
                    }

                    {
                        let state = this.state.lock();
                        json_bytes.clear();
                        serde_json::to_writer(
                            &mut json_bytes,
                            &ClickhouseEventRequestBody {
                                token: ZED_SECRET_CLIENT_TOKEN,
                                installation_id: state.installation_id.clone(),
                                app_version: state.app_version.clone(),
                                os_name: state.os_name,
                                os_version: state.os_version.clone(),
                                architecture: state.architecture,

                                release_channel: state.release_channel,
                                events,
                            },
                        )?;
                    }

                    this.http_client
                        .post_json(CLICKHOUSE_EVENTS_URL.as_str(), json_bytes.into())
                        .await?;
                    anyhow::Ok(())
                }
                .log_err(),
            )
            .detach();
    }
}
