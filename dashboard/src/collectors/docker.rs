use async_trait::async_trait;
use bollard::Docker;
use bollard::container::ListContainersOptions;
use chrono::Utc;
use nucleus_core::health::{HealthCheck, Snapshot, Status};

pub struct DockerCheck {
    id: String,
}

impl DockerCheck {
    pub fn new() -> Self {
        Self { id: "docker".into() }
    }
}

#[async_trait]
impl HealthCheck for DockerCheck {
    fn id(&self) -> &str { &self.id }

    async fn probe(&self) -> Snapshot {
        let now = Utc::now();
        let docker = match Docker::connect_with_local_defaults() {
            Ok(d) => d,
            Err(e) => {
                return Snapshot {
                    id: self.id.clone(),
                    status: Status::Down,
                    message: Some(format!("connect: {}", e)),
                    checked_at: now,
                };
            }
        };
        let opts = ListContainersOptions::<String> { all: true, ..Default::default() };
        match docker.list_containers(Some(opts)).await {
            Ok(list) => {
                let total = list.len();
                let running = list.iter().filter(|c| c.state.as_deref() == Some("running")).count();
                Snapshot {
                    id: self.id.clone(),
                    status: if total == 0 { Status::Unknown } else if running == total { Status::Ok } else { Status::Degraded },
                    message: Some(format!("{}/{} running", running, total)),
                    checked_at: now,
                }
            }
            Err(e) => Snapshot {
                id: self.id.clone(),
                status: Status::Down,
                message: Some(format!("list: {}", e)),
                checked_at: now,
            },
        }
    }
}

#[derive(serde::Serialize)]
pub struct ContainerSummary {
    pub id: String,
    pub name: String,
    pub image: String,
    pub state: String,
    pub status: String,
}

pub async fn list_summaries() -> anyhow::Result<Vec<ContainerSummary>> {
    let docker = Docker::connect_with_local_defaults()?;
    let opts = ListContainersOptions::<String> { all: true, ..Default::default() };
    let raw = docker.list_containers(Some(opts)).await?;
    Ok(raw.into_iter().map(|c| {
        let raw_name = c.names.unwrap_or_default().into_iter().next().unwrap_or_default()
            .trim_start_matches('/').to_string();
        let image = c.image.unwrap_or_default();
        ContainerSummary {
            id: c.id.unwrap_or_default().chars().take(12).collect(),
            name: friendly_name(&raw_name, &image),
            image,
            state: c.state.unwrap_or_default(),
            status: c.status.unwrap_or_default(),
        }
    }).collect())
}

/// Docker assigns names like `suspicious_lehmann` to containers started
/// without `--name`. Those leak into the dashboard's container list and
/// make it useless ("which one was that Java probe?"). When we see that
/// pattern, derive a name from the image instead so the UI stays
/// readable even when something was started ad-hoc.
fn friendly_name(name: &str, image: &str) -> String {
    if !is_docker_random_name(name) {
        return name.to_string();
    }
    let stripped = image.split('@').next().unwrap_or(image);  // strip @sha256:...
    let stripped = stripped.split(':').next().unwrap_or(stripped);  // strip :tag
    let basename = stripped.rsplit('/').next().unwrap_or(stripped);  // last path segment
    if basename.is_empty() || is_image_id(basename) {
        format!("unnamed-{}", &basename[..basename.len().min(8)])
    } else {
        basename.to_string()
    }
}

/// Two lowercase-letter words joined by an underscore — docker's
/// moby/moby/pkg/namesgenerator output. `cool_swirles`, `funny_einstein`, …
fn is_docker_random_name(name: &str) -> bool {
    let mut parts = name.split('_');
    let (Some(a), Some(b), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let lowercase_word = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_lowercase());
    lowercase_word(a) && lowercase_word(b)
}

fn is_image_id(s: &str) -> bool {
    s.len() >= 12 && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[derive(serde::Serialize)]
pub struct ContainerDetail {
    pub id: String,
    pub name: String,
    pub image: String,
    pub state: String,
    pub status: String,
    pub created: Option<String>,
    pub started_at: Option<String>,
    pub ports: Vec<PortMapping>,
    pub processes: Vec<ProcessRow>,
    pub recent_logs: String,
}

#[derive(serde::Serialize)]
pub struct PortMapping {
    pub container_port: String,
    pub host_port: Option<String>,
    pub protocol: String,
}

#[derive(serde::Serialize)]
pub struct ProcessRow {
    pub user: String,
    pub pid: String,
    pub cmd: String,
}

pub async fn detail(id_prefix: &str) -> anyhow::Result<ContainerDetail> {
    let docker = Docker::connect_with_local_defaults()?;
    // Resolve the full id (the UI passes a 12-char prefix).
    let opts = ListContainersOptions::<String> { all: true, ..Default::default() };
    let list = docker.list_containers(Some(opts)).await?;
    let full_id = list.iter()
        .find_map(|c| {
            let cid = c.id.clone().unwrap_or_default();
            if cid.starts_with(id_prefix) { Some(cid) } else { None }
        })
        .ok_or_else(|| anyhow::anyhow!("no container starting with {}", id_prefix))?;

    let inspect = docker.inspect_container(&full_id, None).await?;
    let raw_name = inspect.name.unwrap_or_default().trim_start_matches('/').to_string();
    let image = inspect.config.as_ref().and_then(|c| c.image.clone()).unwrap_or_default();
    let name = friendly_name(&raw_name, &image);
    let state_obj = inspect.state.as_ref();
    let state = state_obj.and_then(|s| s.status.map(|st| format!("{:?}", st).to_lowercase())).unwrap_or_default();
    let status = state_obj.and_then(|s| s.error.clone()).unwrap_or_default();
    let created = inspect.created.clone();
    let started_at = state_obj.and_then(|s| s.started_at.clone());

    // Ports.
    let mut ports = Vec::new();
    if let Some(net) = &inspect.network_settings {
        if let Some(port_map) = &net.ports {
            for (k, v) in port_map.iter() {
                let (cport, proto) = if let Some(idx) = k.find('/') {
                    (k[..idx].to_string(), k[idx + 1..].to_string())
                } else {
                    (k.clone(), "tcp".to_string())
                };
                let host = v.as_ref().and_then(|bindings| {
                    bindings.first().and_then(|b| b.host_port.clone())
                });
                ports.push(PortMapping { container_port: cport, host_port: host, protocol: proto });
            }
        }
    }

    // Processes via `docker top`.
    let processes = match docker.top_processes::<&str>(&full_id, None).await {
        Ok(top) => {
            let titles = top.titles.unwrap_or_default();
            let user_idx = titles.iter().position(|t| t == "USER" || t == "UID");
            let pid_idx = titles.iter().position(|t| t == "PID");
            let cmd_idx = titles.iter().position(|t| t == "CMD" || t == "COMMAND");
            top.processes.unwrap_or_default().into_iter().map(|row| ProcessRow {
                user: pick(&row, user_idx),
                pid: pick(&row, pid_idx),
                cmd: pick(&row, cmd_idx),
            }).collect()
        }
        Err(_) => vec![],
    };

    // Recent logs (last 50 lines, stdout+stderr, no follow).
    let recent_logs = fetch_logs(&docker, &full_id).await.unwrap_or_default();

    Ok(ContainerDetail {
        id: full_id.chars().take(12).collect(),
        name,
        image,
        state,
        status,
        created,
        started_at,
        ports,
        processes,
        recent_logs,
    })
}

fn pick(row: &[String], idx: Option<usize>) -> String {
    idx.and_then(|i| row.get(i).cloned()).unwrap_or_default()
}

async fn fetch_logs(docker: &Docker, full_id: &str) -> anyhow::Result<String> {
    use bollard::container::LogsOptions;
    use futures::StreamExt;
    let opts = LogsOptions::<String> {
        stdout: true,
        stderr: true,
        tail: "50".into(),
        timestamps: false,
        ..Default::default()
    };
    let mut stream = docker.logs(full_id, Some(opts));
    let mut buf = String::new();
    while let Some(chunk) = stream.next().await {
        if let Ok(c) = chunk {
            buf.push_str(&c.to_string());
        }
        if buf.len() > 8 * 1024 { break; }
    }
    Ok(buf)
}
