//! Microsoft sign-in for a device with no launcher (iOS): the OAuth2 device-code flow
//! PommeMC's client uses on phones. The game shows a code, the player enters it at
//! microsoft.com/link in any browser, and this polls until they have; the token goes through Xbox
//! Live and XSTS to a Minecraft services token and the player's profile. What the player needs
//! afterwards (the account and its refresh token) is kept in the app's Library folder, which the
//! Files app does not show, and read back at the next start. The access token is never logged.
use crate::auth::{self, AuthSession};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// The application (client) id the device-code sign-in runs as. PommeMC's client registered this
/// one for exactly this flow; `HORIZON_MS_CLIENT_ID` (or `microsoft-client-id.txt` beside the
/// account file) replaces it with one of the player's own.
const DEFAULT_CLIENT_ID: &str = "b2dd2c0f-8a09-4549-9d76-ab43b8572695";
const SCOPE: &str = "XboxLive.signin offline_access";
const DEVICE_CODE_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode";
const TOKEN_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// A saved Minecraft token with less than this left is refreshed at the start.
const REFRESH_MARGIN: u64 = 3600;

/// Where the sign-in stands, for the screen.
#[derive(Clone, Debug, PartialEq)]
pub enum State {
    /// Asking Microsoft for a code.
    Starting,
    /// The player enters `user_code` at `uri`.
    Code { user_code: String, uri: String },
    /// The code was accepted: Xbox Live, XSTS and Minecraft services follow.
    Working,
    /// Signed in as this player.
    Done(String),
    Failed(String),
}

/// A sign-in running on its own thread.
#[derive(Clone)]
pub struct Login {
    state: Arc<Mutex<State>>,
    cancel: Arc<AtomicBool>,
}

impl Login {
    pub fn start() -> Self {
        let login = Self {
            state: Arc::new(Mutex::new(State::Starting)),
            cancel: Arc::new(AtomicBool::new(false)),
        };
        let this = login.clone();
        let spawned = std::thread::Builder::new()
            .name("Microsoft sign-in".into())
            .spawn(move || {
                let end = match this.run() {
                    Ok(name) => State::Done(name),
                    Err(e) => State::Failed(e),
                };
                this.set(end);
            });
        if let Err(e) = spawned {
            login.set(State::Failed(format!("could not start: {e}")));
        }
        login
    }

    pub fn state(&self) -> State {
        self.state
            .lock()
            .map_or_else(|e| e.into_inner().clone(), |s| s.clone())
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    fn set(&self, state: State) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = state;
    }

    fn run(&self) -> Result<String, String> {
        let agent = agent();
        let id = client_id();
        let (status, body) = call(
            agent
                .post(DEVICE_CODE_URL)
                .send_form(&[("client_id", &id), ("scope", SCOPE)]),
        )?;
        if status != 200 {
            return Err(oauth_error(&body));
        }
        let text = |key: &str| body.get(key).and_then(Value::as_str).map(str::to_owned);
        let (Some(user_code), Some(device_code)) = (text("user_code"), text("device_code")) else {
            return Err("Microsoft's answer had no sign-in code".into());
        };
        let uri = text("verification_uri").unwrap_or_else(|| "https://microsoft.com/link".into());
        let expires = body.get("expires_in").and_then(Value::as_u64).unwrap_or(900);
        let mut interval = Duration::from_secs(
            body.get("interval").and_then(Value::as_u64).unwrap_or(5).max(1),
        );
        self.set(State::Code { user_code, uri });
        let deadline = std::time::Instant::now() + Duration::from_secs(expires);
        while std::time::Instant::now() < deadline {
            // Sleep in small steps: a cancel should not wait out the interval.
            let wake = std::time::Instant::now() + interval;
            while std::time::Instant::now() < wake {
                if self.cancel.load(Ordering::Relaxed) {
                    return Err("cancelled".into());
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            // A poll can fail without the sign-in failing (the app was in the background).
            let Ok((status, body)) = call(agent.post(TOKEN_URL).send_form(&[
                ("client_id", &id),
                ("grant_type", DEVICE_GRANT),
                ("device_code", &device_code),
            ])) else {
                continue;
            };
            if status == 200 {
                self.set(State::Working);
                let token = |k: &str| body.get(k).and_then(Value::as_str).map(str::to_owned);
                let access = token("access_token").ok_or("Microsoft's answer had no token")?;
                return finish(&agent, &access, token("refresh_token"));
            }
            match poll_outcome(&body) {
                Poll::Pending => {}
                Poll::SlowDown => interval += Duration::from_secs(5),
                Poll::Failed(reason) => return Err(reason),
            }
        }
        Err("The code expired; try again".into())
    }
}

enum Poll {
    Pending,
    SlowDown,
    Failed(String),
}

fn poll_outcome(body: &Value) -> Poll {
    match body.get("error").and_then(Value::as_str) {
        Some("authorization_pending") => Poll::Pending,
        Some("slow_down") => Poll::SlowDown,
        Some("authorization_declined") => Poll::Failed("Sign-in was declined".into()),
        Some("expired_token") => Poll::Failed("The code expired; try again".into()),
        _ => Poll::Failed(oauth_error(body)),
    }
}

/// Microsoft's own wording of an OAuth error.
fn oauth_error(body: &Value) -> String {
    let field = |k: &str| body.get(k).and_then(Value::as_str);
    match (field("error_description"), field("error")) {
        // The description ends with trace ids on further lines.
        (Some(d), _) => d.lines().next().unwrap_or(d).to_owned(),
        (None, Some(e)) => e.to_owned(),
        _ => "Microsoft refused the sign-in".to_owned(),
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout_read(Duration::from_secs(30))
        .user_agent("Horizon-iOS")
        .build()
}

/// A response's status and JSON body, whether the status is a success or not: the error
/// replies hold what to tell the player. Only a failure to reach the server is an `Err`.
fn call(result: Result<ureq::Response, ureq::Error>) -> Result<(u16, Value), String> {
    let (status, response) = match result {
        Ok(r) => (r.status(), r),
        Err(ureq::Error::Status(code, r)) => (code, r),
        Err(e) => return Err(format!("Could not reach Microsoft: {e}")),
    };
    let text = response
        .into_string()
        .map_err(|e| format!("Could not read the answer: {e}"))?;
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
}

fn post_json(agent: &ureq::Agent, url: &str, body: &Value) -> Result<(u16, Value), String> {
    call(
        agent
            .post(url)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json")
            .send_string(&body.to_string()),
    )
}

/// The Xbox Live and XSTS refusals worth explaining.
fn xsts_error(body: &Value) -> String {
    match body.get("XErr").and_then(Value::as_u64) {
        Some(2148916233) => "This Microsoft account has no Xbox profile: sign in at minecraft.net once first".into(),
        Some(2148916235) => "Xbox Live is not available in this country".into(),
        Some(2148916236 | 2148916237) => "This account needs adult verification on the Xbox site".into(),
        Some(2148916238) => "A child account must be added to a Family to play".into(),
        Some(code) => format!("Xbox Live refused the sign-in ({code})"),
        None => "Xbox Live refused the sign-in".into(),
    }
}

/// The Microsoft access token, through Xbox Live and XSTS, to a Minecraft account; saved.
fn finish(agent: &ureq::Agent, msa_token: &str, refresh: Option<String>) -> Result<String, String> {
    let (status, xbl) = post_json(
        agent,
        "https://user.auth.xboxlive.com/user/authenticate",
        &json!({
            "Properties": {
                "AuthMethod": "RPS",
                "SiteName": "user.auth.xboxlive.com",
                "RpsTicket": format!("d={msa_token}"),
            },
            "RelyingParty": "http://auth.xboxlive.com",
            "TokenType": "JWT",
        }),
    )?;
    if status != 200 {
        return Err(xsts_error(&xbl));
    }
    let xbl_token = xbl.get("Token").and_then(Value::as_str).ok_or("Xbox Live gave no token")?;
    let user_hash = xbl
        .pointer("/DisplayClaims/xui/0/uhs")
        .and_then(Value::as_str)
        .ok_or("Xbox Live gave no user hash")?;
    let (status, xsts) = post_json(
        agent,
        "https://xsts.auth.xboxlive.com/xsts/authorize",
        &json!({
            "Properties": {"SandboxId": "RETAIL", "UserTokens": [xbl_token]},
            "RelyingParty": "rp://api.minecraftservices.com/",
            "TokenType": "JWT",
        }),
    )?;
    if status != 200 {
        return Err(xsts_error(&xsts));
    }
    let xsts_token = xsts.get("Token").and_then(Value::as_str).ok_or("XSTS gave no token")?;
    let (status, mc) = post_json(
        agent,
        "https://api.minecraftservices.com/authentication/login_with_xbox",
        &json!({"identityToken": format!("XBL3.0 x={user_hash};{xsts_token}")}),
    )?;
    if status != 200 {
        return Err(format!("Minecraft services refused the sign-in ({status})"));
    }
    let access = mc
        .get("access_token")
        .and_then(Value::as_str)
        .ok_or("Minecraft services gave no token")?
        .to_owned();
    let lifetime = mc.get("expires_in").and_then(Value::as_u64).unwrap_or(86_400);
    let (status, profile) = call(
        agent
            .get("https://api.minecraftservices.com/minecraft/profile")
            .set("Authorization", &format!("Bearer {access}"))
            .call(),
    )?;
    if status == 404 {
        return Err("This Microsoft account doesn't own Minecraft: Java Edition".into());
    }
    if status != 200 {
        return Err(format!("Minecraft services refused the profile ({status})"));
    }
    let name = profile.get("name").and_then(Value::as_str).ok_or("The profile had no name")?;
    let id = profile.get("id").and_then(Value::as_str).ok_or("The profile had no id")?;
    let account = Account {
        name: name.to_owned(),
        uuid: id.to_owned(),
        access_token: access,
        expires_at: unix_now() + lifetime,
        refresh_token: refresh,
    };
    apply(&account)?;
    account.save();
    Ok(account.name)
}

/// The signed-in account as kept on disk.
#[derive(Clone, Debug, PartialEq)]
struct Account {
    name: String,
    /// Dashed or not.
    uuid: String,
    access_token: String,
    /// Unix seconds.
    expires_at: u64,
    refresh_token: Option<String>,
}

impl Account {
    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "uuid": self.uuid,
            "access_token": self.access_token,
            "expires_at": self.expires_at,
            "refresh_token": self.refresh_token,
        })
    }

    fn from_json(v: &Value) -> Option<Self> {
        let text = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        Some(Self {
            name: text("name")?,
            uuid: text("uuid")?,
            access_token: text("access_token")?,
            expires_at: v.get("expires_at").and_then(Value::as_u64).unwrap_or(0),
            refresh_token: text("refresh_token"),
        })
    }

    fn load() -> Option<Self> {
        let text = std::fs::read_to_string(account_path()).ok()?;
        Self::from_json(&serde_json::from_str(&text).ok()?)
    }

    /// Written for its owner only. A failure only means signing in again next time.
    fn save(&self) {
        let path = account_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let write = || -> std::io::Result<()> {
            use std::io::Write;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            options.open(&path)?.write_all(self.to_json().to_string().as_bytes())
        };
        if let Err(e) = write() {
            log::warn!("Horizon: could not keep the Microsoft account: {e}");
        }
    }

    fn fresh(&self) -> bool {
        self.expires_at > unix_now() + REFRESH_MARGIN
    }
}

/// Make `account` the game's signed-in player.
fn apply(account: &Account) -> Result<(), String> {
    let uuid = auth::parse_uuid(&account.uuid).ok_or("The profile's id is not a UUID")?;
    auth::set_current(AuthSession::new(&account.name, uuid, &account.access_token));
    // The chat key pair, ready before the first join.
    auth::prepare_chat_keys(true);
    Ok(())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The app's Library folder on iOS (which the Files app does not show), else the player folder.
fn account_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|_| cfg!(target_os = "ios"))
        .map(|home| home.join("Library"))
        .unwrap_or_else(crate::logging::player_dir)
}

fn account_path() -> PathBuf {
    account_dir().join("horizon-microsoft-account.json")
}

fn client_id() -> String {
    std::env::var("HORIZON_MS_CLIENT_ID")
        .ok()
        .or_else(|| std::fs::read_to_string(account_dir().join("microsoft-client-id.txt")).ok())
        .map(|id| id.trim().to_owned())
        .filter(|id| {
            !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
        })
        .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_owned())
}

/// The name of the account kept from an earlier sign-in, if there is one.
pub fn saved_name() -> Option<String> {
    Account::load().map(|a| a.name)
}

/// Forget the kept account and the signed-in player.
pub fn sign_out() {
    let _ = std::fs::remove_file(account_path());
    auth::clear_current();
}

/// At the start: sign in again as the account kept earlier, refreshing its token when it is near
/// its end, on a thread: the game does not wait for the network. Does nothing without one.
pub fn restore() {
    let Some(account) = Account::load() else {
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("Microsoft restore".into())
        .spawn(move || {
            if account.fresh() {
                if apply(&account).is_ok() {
                    log::info!("Horizon: signed in as {}", account.name);
                }
                return;
            }
            let Some(refresh) = account.refresh_token.clone() else {
                log::info!("Horizon: the kept Microsoft sign-in expired; sign in again");
                return;
            };
            let agent = agent();
            let result = call(agent.post(TOKEN_URL).send_form(&[
                ("client_id", &client_id()),
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh),
                ("scope", SCOPE),
            ]))
            .and_then(|(status, body)| {
                if status != 200 {
                    return Err(oauth_error(&body));
                }
                let token = |k: &str| body.get(k).and_then(Value::as_str).map(str::to_owned);
                let access = token("access_token").ok_or("Microsoft's answer had no token")?;
                // A refresh token is replaced by a new one; keep the old if none came.
                finish(&agent, &access, token("refresh_token").or(Some(refresh)))
            });
            match result {
                Ok(name) => log::info!("Horizon: signed in again as {name}"),
                Err(e) => log::warn!("Horizon: the kept Microsoft sign-in did not refresh: {e}"),
            }
        });
    if let Err(e) = spawned {
        log::warn!("Horizon: could not start the Microsoft sign-in restore: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_errors_map_to_their_outcome() {
        let outcome = |text: &str| poll_outcome(&serde_json::from_str(text).unwrap());
        assert!(matches!(
            outcome(r#"{"error":"authorization_pending"}"#),
            Poll::Pending
        ));
        assert!(matches!(outcome(r#"{"error":"slow_down"}"#), Poll::SlowDown));
        assert!(matches!(
            outcome(r#"{"error":"expired_token"}"#),
            Poll::Failed(m) if m.contains("expired")
        ));
        assert!(matches!(
            outcome(r#"{"error":"invalid_grant","error_description":"bad\r\nTrace ID: 1"}"#),
            Poll::Failed(m) if m == "bad"
        ));
    }

    #[test]
    fn xbox_refusals_are_explained() {
        let said = |code: u64| xsts_error(&json!({ "XErr": code }));
        assert!(said(2148916233).contains("Xbox profile"));
        assert!(said(2148916238).contains("Family"));
        assert!(said(1).contains("(1)"));
        assert_eq!(xsts_error(&Value::Null), "Xbox Live refused the sign-in");
    }

    #[test]
    fn accounts_round_trip_and_expire() {
        let account = Account {
            name: "Steve".into(),
            uuid: "069a79f444e94726a5befca90e38aaf5".into(),
            access_token: "secret".into(),
            expires_at: unix_now() + 10 * REFRESH_MARGIN,
            refresh_token: Some("refresh".into()),
        };
        assert_eq!(Account::from_json(&account.to_json()), Some(account.clone()));
        assert!(account.fresh());
        assert!(!Account { expires_at: unix_now() + 60, ..account }.fresh());
        assert_eq!(Account::from_json(&json!({"name": "x"})), None);
    }

    #[test]
    fn a_client_id_must_look_like_one() {
        assert_eq!(DEFAULT_CLIENT_ID.len(), 36);
        assert!(DEFAULT_CLIENT_ID.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
    }
}
