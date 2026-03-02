use axum::{
    extract::{DefaultBodyLimit, Path, State, Multipart},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response}, // Added Response
    routing::{get, post},
    Router,
    Form,
};
use axum::body::Body;
use sysinfo::{System, Disks}; // Updated for sysinfo 0.30+

use axum_extra::extract::cookie::{Cookie, CookieJar};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use tokio::{fs, fs::File, io::AsyncWriteExt, sync::RwLock};
use tokio::io::AsyncReadExt;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

const STORAGE_PATH: &str = "/srv/nas_storage";
const STATE_FILE: &str = "/srv/nas_storage/nas_state.json";

// --- Database Structures ---
#[derive(Serialize, Deserialize, Default, Clone)]
struct NasStateData {
    users: HashMap<String, String>,   // username -> password
    files: HashMap<String, FileMeta>, // filename -> metadata
}

#[derive(Serialize, Deserialize, Clone)]
struct FileMeta {
    owner: String,
    is_public: bool,
}

// Thread-safe state to share across all network requests
type SharedState = Arc<RwLock<NasStateData>>;

fn format_size(bytes: u64) -> String {
    let kb = bytes as f64 / 1024.0;
    let mb = kb / 1024.0;
    let gb = mb / 1024.0;
    if gb >= 1.0 { format!("{:.2} GB", gb) }
    else if mb >= 1.0 { format!("{:.2} MB", mb) }
    else { format!("{:.2} KB", kb) }
}

use mime_guess; // cargo add mime_guess

async fn download_file(
    State(state): State<SharedState>,
    jar: CookieJar,
    Path(filename): Path<String>,
    ) -> impl IntoResponse {
    // ... (keep your permission checks) ...

    let path = std::path::Path::new(STORAGE_PATH).join(&filename);
    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    };

    // Automatically detect if it's a video, image, or text
    let content_type = mime_guess::from_path(&path).first_or_octet_stream();

    let stream = tokio_util::io::ReaderStream::new(file);
    let body = Body::from_stream(stream);

    Response::builder()
        .header("Content-Type", content_type.to_string())
        .header("Content-Disposition", format!("inline; filename=\"{}\"", filename))
        .body(body)
        .unwrap()
        .into_response()
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    // Load existing users and files, or create a blank slate
    let state_data = if let Ok(data) = std::fs::read_to_string(STATE_FILE) {
        serde_json::from_str(&data).unwrap_or_default()
    } else {
        NasStateData::default()
    };
    let state = Arc::new(RwLock::new(state_data));

    let app = Router::new()
        .route("/", get(list_files_html))
        .route("/login", get(login_page).post(login_post))
        .route("/logout", get(logout))
        .route("/upload", post(upload_file))
        .route("/delete/:filename", get(delete_file))
        .route("/download/:filename", get(download_file))
        .route("/toggle_visibility/:filename", get(toggle_visibility))
        .nest_service("/assets", ServeDir::new("/srv/nas_storage/assets"))
        .nest_service("/files", ServeDir::new(STORAGE_PATH)) 
        .layer(DefaultBodyLimit::max(1024 * 1024 * 1024))
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], 3000));
    println!("NAS Server running at http://10.100.128.60:3000");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// --- Helper Functions ---
fn get_current_user(jar: &CookieJar) -> Option<String> {
    jar.get("session").map(|c| c.value().to_string())
}

async fn save_state(state: &NasStateData) {
    if let Ok(json) = serde_json::to_string_pretty(state) {
        let _ = tokio::fs::write(STATE_FILE, json).await;
    }
}

// --- Route Handlers ---

// 1. The Login Page
async fn login_page() -> Html<&'static str> {
    Html(
        r#"
        <html>
            <head><title>Login - Rust NAS</title></head>
            <body style='font-family: sans-serif; padding: 20px; max-width: 400px; margin: auto;'>
                <h2>NAS Login</h2>
                <form action='/login' method='post'>
                    <label>Username (leave blank for anonymous):</label><br/>
                    <input type='text' name='username' style='width:100%; margin-bottom: 10px;'><br/>
                    <label>Password:</label><br/>
                    <input type='password' name='password' style='width:100%; margin-bottom: 10px;'><br/>
                    <input type='submit' value='Login' style='width:100%; padding: 10px;'>
                </form>
                <p style="font-size: 0.8em; color: gray;">*New usernames will be auto-registered securely.</p>
            </body>
        </html>
        "#
        )
}

#[derive(Deserialize)]
struct LoginPayload {
    username: String,
    password: Option<String>,
}

// 2. Process the Login Form
// 2. Process the Login Form
async fn login_post(
    State(state): State<SharedState>,
    jar: CookieJar,
    Form(payload): Form<LoginPayload>,
    ) -> impl IntoResponse {
    let username = payload.username.trim().to_string();
    let password = payload.password.unwrap_or_default();

    let user_to_set = if username.is_empty() {
        "anonymous".to_string()
    } else if username == "admin" {
        if password == "admin" {
            "admin".to_string()
        } else {
            // Standardize return type using .into_response()
            return (jar, Html("Invalid admin password. <a href='/login'>Try again</a>")).into_response();
        }
    } else {
        let mut data = state.write().await;
        if let Some(existing_pass) = data.users.get(&username) {
            if existing_pass != &password {
                return (jar, Html("Invalid password. <a href='/login'>Try again</a>")).into_response();
            }
        } else {
            data.users.insert(username.clone(), password);
            save_state(&data).await;
        }
        username
    };

    let updated_jar = jar.add(Cookie::new("session", user_to_set));
    (updated_jar, Redirect::to("/")).into_response()
}

// 3. Logout
async fn logout(jar: CookieJar) -> impl IntoResponse {
    (jar.remove(Cookie::from("session")), Redirect::to("/login"))
}

// 4. List Files (Enforcing Permissions)
async fn list_files_html(State(state): State<SharedState>, jar: CookieJar) -> impl IntoResponse {
    let current_user = match get_current_user(&jar) {
        Some(u) => u,
        None => return Redirect::to("/login").into_response(),
    };

    // Updated sysinfo 0.30+ logic for disks
    let disks = Disks::new_with_refreshed_list();
    let disk = disks.iter().find(|d| d.mount_point() == std::path::Path::new("/")).unwrap();
    let free_space = format_size(disk.available_space());
    let percent_used = ((disk.total_space() - disk.available_space()) as f64 / disk.total_space() as f64) * 100.0;

    let data = state.read().await;
    let mut rows = String::new();
    let mut entries = fs::read_dir(STORAGE_PATH).await.unwrap();

    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().into_string().unwrap();
        if name == "nas_state.json" || name == "assets" { continue; }

        let meta = entry.metadata().await.unwrap();
        let size_str = format_size(meta.len());

        let file_meta = data.files.get(&name);
        let owner = file_meta.map(|m| m.owner.as_str()).unwrap_or("anonymous");
        let is_public = file_meta.map(|m| m.is_public).unwrap_or(true);

        if is_public || owner == current_user || current_user == "admin" {
            let ext = std::path::Path::new(&name)
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase();

            let raw_bytes = meta.len(); // The actual number
            let size_str = format_size(raw_bytes);

            let visibility_icon = if is_public { "🌐 Public" } else { "🔒 Private" };
            let can_edit = owner == current_user || current_user == "admin" || owner == "anonymous";
            let visibility_cell = if can_edit {
                format!("<a href='/toggle_visibility/{}' style='text-decoration:none;'>{}</a>", name, visibility_icon)
            } else {
                visibility_icon.to_string() // Non-owners just see the text, no link
            };
            rows.push_str(&format!(
                    "<tr>
                    <td><a href='/download/{name}'>{name}</a></td>
                    <td>{owner}</td>
                    <td data-size='{raw_bytes}'>{size_str}</td> 
<td>{visibility_cell}</td>              
      <td> <a href='/delete/{name}' class='btn-delete'>[Delete]</a></td>
                </tr>"
                ));
        }
    }

    // --- FINISH THE FUNCTION BY RETURNING THE HTML ---
    let mut html_template = String::new();
    if let Ok(mut file) = File::open("/srv/nas_storage/assets/index.html").await {
        let _ = file.read_to_string(&mut html_template).await;
    } else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "Missing index.html asset").into_response();
    }

    let final_html = html_template
        .replace("{{username}}", &current_user)
        .replace("{{rows}}", &rows)
        .replace("{{free_space}}", &free_space)
        .replace("{{percent_used}}", &format!("{:.1}", percent_used));

    Html(final_html).into_response()
}
// 5. Stream Uploads to Disk
async fn upload_file(
    State(state): State<SharedState>,
    jar: CookieJar,
    mut multipart: Multipart,
    ) -> Result<Redirect, StatusCode> {
    let current_user = get_current_user(&jar).unwrap_or_else(|| "anonymous".to_string());

    let mut final_file_name = String::new();
    let mut is_public = false;

    // Use a while let loop to process fields
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().map(|n| n.to_string()).unwrap_or_default();

        if name == "is_public" {
            is_public = true;
        } else if name == "data" {
            let mut file_name = field.file_name().map(|n| n.to_string()).unwrap_or_default();

            if file_name.is_empty() {
                file_name = format!("upload_{}", chrono::Utc::now().timestamp());
            }
            if file_name.contains("..") || file_name.contains('/') {
                return Err(StatusCode::BAD_REQUEST);
            }

            final_file_name = file_name.clone();
            let path = std::path::Path::new(STORAGE_PATH).join(&file_name);
            let mut file = File::create(&path).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

            // FIXED: We extract the field's data correctly
            let data = field.bytes().await.map_err(|_| StatusCode::BAD_REQUEST)?;
            file.write_all(&data).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        }
    }

    if !final_file_name.is_empty() {
        let mut data = state.write().await;
        data.files.insert(final_file_name, FileMeta {
            owner: current_user,
            is_public,
        });
        save_state(&data).await;
    }

    Ok(Redirect::to("/"))
}

// 6. Delete File Enforcer
async fn delete_file(
    State(state): State<SharedState>,
    jar: CookieJar,
    Path(filename): Path<String>,
    ) -> Result<Redirect, StatusCode> {
    let current_user = get_current_user(&jar).unwrap_or_else(|| "anonymous".to_string());

    if filename.contains("..") || filename.contains('/') {
        return Err(StatusCode::BAD_REQUEST);
    }

    {
        let data = state.read().await;
        let meta = data.files.get(&filename);
        let owner = meta.map(|m| m.owner.as_str()).unwrap_or("anonymous");

        // Block unauthorized deletion
        if owner != current_user && current_user != "admin" && owner != "anonymous" {
            return Err(StatusCode::FORBIDDEN);
        }
    }

    let path = std::path::Path::new(STORAGE_PATH).join(&filename);
    if fs::remove_file(path).await.is_ok() {
        let mut data = state.write().await;
        data.files.remove(&filename);
        save_state(&data).await;
        Ok(Redirect::to("/"))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}
async fn toggle_visibility(
    State(state): State<SharedState>,
    jar: CookieJar,
    Path(filename): Path<String>,
    ) -> Result<Redirect, StatusCode> {
    let current_user = get_current_user(&jar).unwrap_or_else(|| "anonymous".to_string());

    if filename.contains("..") || filename.contains('/') {
        return Err(StatusCode::BAD_REQUEST);
    }

    let mut data = state.write().await;

    // Check if the user has permission to change this file
    let can_edit = if let Some(meta) = data.files.get(&filename) {
        meta.owner == current_user || current_user == "admin" || meta.owner == "anonymous"
    } else {
        return Err(StatusCode::NOT_FOUND);
    };

    if !can_edit {
        return Err(StatusCode::FORBIDDEN);
    }

    // Flip the boolean
    if let Some(meta) = data.files.get_mut(&filename) {
        meta.is_public = !meta.is_public;
    }

    save_state(&data).await;

    // Refresh the page
    Ok(Redirect::to("/"))
}
