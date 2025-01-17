use serde_json::json;
use zino::{prelude::*, Cluster, Request, Response, Result};

pub async fn index(req: Request) -> Result {
    let res = Response::default().context(&req);
    let stats = json!({
        "method": "GET",
        "path": "/stats",
        "app_state_data": Cluster::state_data(),
        "app_sysinfo": Cluster::sysinfo(),
    });
    let data = json!({
        "title": "Stats",
        "output": serde_json::to_string_pretty(&stats).unwrap_or_default(),
    });
    Ok(res.render("output.html", data).into())
}
