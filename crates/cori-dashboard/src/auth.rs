//! Authentication middleware for the dashboard.
//!
//! Provides form-based authentication with session cookies when configured in cori.yaml.
//! If no password is configured, authentication is skipped.

use axum::{
    Form,
    extract::{Request, State},
    http::header,
    middleware::Next,
    response::{Html, IntoResponse, Redirect, Response},
};
use base64::Engine;
use serde::Deserialize;

use crate::state::AppState;

/// Session cookie name
const SESSION_COOKIE_NAME: &str = "cori_session";

/// Session token prefix for validation
const SESSION_PREFIX: &str = "cori_auth_";

/// Basic auth middleware that checks credentials if authentication is configured.
/// If no authentication is configured (no password set), requests are allowed through.
pub async fn auth_middleware(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    // Get auth config from state - clone to avoid holding lock across await
    let auth_config = {
        let config = state.config();
        config.dashboard.auth.clone()
    };

    // If auth is not configured (no password set), allow the request through
    if !auth_config.is_configured() {
        return next.run(request).await;
    }

    let path = request.uri().path();
    
    // Allow access to login page and static assets without authentication
    if path == "/login" || path.starts_with("/api/login") {
        return next.run(request).await;
    }

    // Check for valid session cookie
    if let Some(cookie_header) = request.headers().get(header::COOKIE) {
        if let Ok(cookies) = cookie_header.to_str() {
            if let Some(session) = extract_session_cookie(cookies) {
                if validate_session(&session) {
                    return next.run(request).await;
                }
            }
        }
    }

    // No valid session, redirect to login page
    let redirect_to = request.uri().path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    
    Redirect::to(&format!("/login?redirect={}", urlencoding::encode(redirect_to))).into_response()
}

/// Extract session cookie value from cookie header
fn extract_session_cookie(cookies: &str) -> Option<String> {
    for cookie in cookies.split(';') {
        let cookie = cookie.trim();
        if let Some(value) = cookie.strip_prefix(&format!("{}=", SESSION_COOKIE_NAME)) {
            return Some(value.to_string());
        }
    }
    None
}

/// Validate a session token
fn validate_session(session: &str) -> bool {
    // Simple validation: check prefix and decode
    if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(session) {
        if let Ok(token) = String::from_utf8(decoded) {
            return token.starts_with(SESSION_PREFIX);
        }
    }
    false
}

/// Create a session token for a user
fn create_session_token(username: &str) -> String {
    let token = format!("{}{}", SESSION_PREFIX, username);
    base64::engine::general_purpose::STANDARD.encode(token)
}

/// Login page query parameters
#[derive(Debug, Deserialize)]
pub struct LoginQuery {
    #[serde(default)]
    pub redirect: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

/// Login form data
#[derive(Debug, Deserialize)]
pub struct LoginForm {
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub redirect: Option<String>,
}

/// Handler for the login page (GET)
pub async fn login_page(
    axum::extract::Query(query): axum::extract::Query<LoginQuery>,
) -> Html<String> {
    Html(login_page_template(query.redirect.as_deref(), query.error.as_deref()))
}

/// Handler for login form submission (POST)
pub async fn login_submit(
    State(state): State<AppState>,
    Form(form): Form<LoginForm>,
) -> Response {
    // Get auth config
    let auth_config = {
        let config = state.config();
        config.dashboard.auth.clone()
    };

    // Validate credentials
    if let Some(username) = auth_config.validate_basic_auth(&form.username, &form.password) {
        // Create session token
        let session_token = create_session_token(&username);
        
        // Determine redirect URL
        let redirect_url = form.redirect
            .filter(|r| !r.is_empty() && r.starts_with('/'))
            .unwrap_or_else(|| "/".to_string());
        
        // Set cookie and redirect
        let cookie = format!(
            "{}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age=86400",
            SESSION_COOKIE_NAME, session_token
        );
        
        (
            [(header::SET_COOKIE, cookie)],
            Redirect::to(&redirect_url),
        ).into_response()
    } else {
        // Invalid credentials, redirect back to login with error
        let redirect = form.redirect
            .map(|r| format!("&redirect={}", urlencoding::encode(&r)))
            .unwrap_or_default();
        
        Redirect::to(&format!("/login?error=invalid{}", redirect)).into_response()
    }
}

/// Handler for logout
pub async fn logout() -> Response {
    // Clear the session cookie
    let cookie = format!(
        "{}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0",
        SESSION_COOKIE_NAME
    );
    
    (
        [(header::SET_COOKIE, cookie)],
        Redirect::to("/login"),
    ).into_response()
}

/// Generate the login page HTML
fn login_page_template(redirect: Option<&str>, error: Option<&str>) -> String {
    let error_html = if error.is_some() {
        r##"<div class="mb-6 p-4 bg-red-50 dark:bg-red-900/20 border border-red-200 dark:border-red-800 rounded-lg">
            <div class="flex items-center gap-3">
                <i class="fas fa-exclamation-circle text-red-500"></i>
                <span class="text-red-700 dark:text-red-400">Invalid username or password. Please try again.</span>
            </div>
        </div>"##
    } else {
        ""
    };

    let redirect_input = redirect
        .map(|r| format!(r#"<input type="hidden" name="redirect" value="{}">"#, html_escape(r)))
        .unwrap_or_default();

    format!(
        r##"<!DOCTYPE html>
<html lang="en" x-data="{{
    darkMode: localStorage.getItem('darkMode') === 'true'
}}" :class="{{ 'dark': darkMode }}">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Login - Cori Dashboard</title>
    <script src="https://cdn.tailwindcss.com"></script>
    <script>
        tailwind.config = {{
            darkMode: 'class',
            theme: {{
                extend: {{
                    colors: {{
                        primary: {{
                            50: '#eef2ff',
                            100: '#e0e7ff',
                            200: '#c7d2fe',
                            300: '#a5b4fc',
                            400: '#818cf8',
                            500: '#6366f1',
                            600: '#4f46e5',
                            700: '#4338ca',
                            800: '#3730a3',
                            900: '#312e81',
                        }}
                    }}
                }}
            }}
        }}
    </script>
    <script defer src="https://unpkg.com/alpinejs@3.x.x/dist/cdn.min.js"></script>
    <link rel="stylesheet" href="https://cdnjs.cloudflare.com/ajax/libs/font-awesome/6.5.1/css/all.min.css">
</head>
<body class="bg-gradient-to-br from-primary-600 via-primary-700 to-primary-900 dark:from-gray-900 dark:via-gray-800 dark:to-gray-900 min-h-screen flex items-center justify-center p-4">
    <div class="absolute top-4 right-4">
        <button @click="darkMode = !darkMode; localStorage.setItem('darkMode', darkMode)"
                class="p-3 bg-white/10 hover:bg-white/20 rounded-full text-white transition-colors">
            <i class="fas" :class="darkMode ? 'fa-sun' : 'fa-moon'"></i>
        </button>
    </div>

    <div class="w-full max-w-md">
        <!-- Logo and Title -->
        <div class="text-center mb-8">
            <div class="inline-flex items-center justify-center h-12 mb-4">
                <img src="https://assets.cori.do/cori-logo-white.png" alt="Cori" class="h-12">
            </div>
            <p class="text-primary-200 dark:text-gray-400">Governed data for AI agents</p>
        </div>

        <!-- Login Card -->
        <div class="bg-white dark:bg-gray-800 rounded-2xl shadow-2xl p-8">
            <div class="text-center mb-6">
                <h2 class="text-xl font-semibold text-gray-900 dark:text-white">Welcome back</h2>
                <p class="text-gray-500 dark:text-gray-400 mt-1">Sign in to access the dashboard</p>
            </div>

            {error_html}

            <form method="POST" action="/login" class="space-y-5">
                {redirect_input}
                
                <div>
                    <label for="username" class="block text-sm font-medium text-gray-700 dark:text-gray-300 mb-2">
                        Username
                    </label>
                    <div class="relative">
                        <div class="absolute inset-y-0 left-0 pl-3 flex items-center pointer-events-none">
                            <i class="fas fa-user text-gray-400"></i>
                        </div>
                        <input type="text" id="username" name="username" required autofocus
                            class="block w-full pl-10 pr-4 py-3 border border-gray-300 dark:border-gray-600 rounded-lg 
                                   bg-white dark:bg-gray-700 text-gray-900 dark:text-white
                                   placeholder-gray-400 dark:placeholder-gray-500
                                   focus:ring-2 focus:ring-primary-500 focus:border-primary-500
                                   transition-colors"
                            placeholder="Enter your username">
                    </div>
                </div>

                <div>
                    <label for="password" class="block text-sm font-medium text-gray-700 dark:text-gray-300 mb-2">
                        Password
                    </label>
                    <div class="relative" x-data="{{ show: false }}">
                        <div class="absolute inset-y-0 left-0 pl-3 flex items-center pointer-events-none">
                            <i class="fas fa-lock text-gray-400"></i>
                        </div>
                        <input :type="show ? 'text' : 'password'" id="password" name="password" required
                            class="block w-full pl-10 pr-12 py-3 border border-gray-300 dark:border-gray-600 rounded-lg 
                                   bg-white dark:bg-gray-700 text-gray-900 dark:text-white
                                   placeholder-gray-400 dark:placeholder-gray-500
                                   focus:ring-2 focus:ring-primary-500 focus:border-primary-500
                                   transition-colors"
                            placeholder="Enter your password">
                        <button type="button" @click="show = !show"
                            class="absolute inset-y-0 right-0 pr-3 flex items-center text-gray-400 hover:text-gray-600 dark:hover:text-gray-300">
                            <i class="fas" :class="show ? 'fa-eye-slash' : 'fa-eye'"></i>
                        </button>
                    </div>
                </div>

                <button type="submit"
                    class="w-full py-3 px-4 bg-primary-600 hover:bg-primary-700 text-white font-medium rounded-lg
                           shadow-lg shadow-primary-500/30 hover:shadow-primary-500/40
                           transform hover:-translate-y-0.5 transition-all duration-200
                           focus:outline-none focus:ring-2 focus:ring-primary-500 focus:ring-offset-2 dark:focus:ring-offset-gray-800">
                    <span class="flex items-center justify-center gap-2">
                        <i class="fas fa-sign-in-alt"></i>
                        Sign In
                    </span>
                </button>
            </form>
        </div>

        <!-- Footer -->
        <div class="text-center mt-6 text-primary-200 dark:text-gray-500 text-sm">
            <p>Protected by Cori · <a href="https://cori.do" class="hover:text-white dark:hover:text-gray-300 transition-colors">Learn more</a></p>
        </div>
    </div>

    <!-- Decorative elements -->
    <div class="fixed inset-0 -z-10 overflow-hidden pointer-events-none">
        <div class="absolute -top-1/2 -right-1/2 w-full h-full bg-gradient-to-b from-primary-400/20 to-transparent rounded-full blur-3xl"></div>
        <div class="absolute -bottom-1/2 -left-1/2 w-full h-full bg-gradient-to-t from-primary-800/20 to-transparent rounded-full blur-3xl"></div>
    </div>
</body>
</html>"##,
        error_html = error_html,
        redirect_input = redirect_input,
    )
}

/// Simple HTML escape function
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}
