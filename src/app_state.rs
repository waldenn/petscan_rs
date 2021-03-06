use crate::datasource::SQLtuple;
use crate::form_parameters::FormParameters;
use crate::platform::{ContentType, MyResponse};
use chrono::prelude::*;
use mysql as my;
use rand::seq::SliceRandom;
use rayon::prelude::*;
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::sync::{Arc, Mutex, RwLock};
use std::{thread, time};
use wikibase::mediawiki::api::Api;

static MAX_CONCURRENT_DB_CONNECTIONS: u64 = 10;
static MYSQL_MAX_CONNECTION_ATTEMPTS: u64 = 15;
static MYSQL_CONNECTION_INITIAL_DELAY_MS: u64 = 100;
static MYSQL_CONNECTION_MAX_DELAY_MS: u64 = 5000;

pub type DbUserPass = (String, String);

#[derive(Debug, Clone)]
pub struct AppState {
    pub db_pool: Vec<Arc<Mutex<DbUserPass>>>,
    pub config: Value,
    tool_db_mutex: Arc<Mutex<DbUserPass>>,
    threads_running: Arc<RwLock<i64>>,
    shutting_down: Arc<RwLock<bool>>,
    site_matrix: Value,
    main_page: String,
}

impl AppState {
    pub fn new_from_config(config: &Value) -> Self {
        let main_page_path = "./html/index.html";
        let tool_db_access_tuple = (
            config["user"]
                .as_str()
                .expect("No user key in config file")
                .to_string(),
            config["password"]
                .as_str()
                .expect("No password key in config file")
                .to_string(),
        );
        let mut ret = Self {
            db_pool: vec![],
            config: config.to_owned(),
            threads_running: Arc::new(RwLock::new(0)),
            shutting_down: Arc::new(RwLock::new(false)),
            site_matrix: AppState::load_site_matrix(),
            tool_db_mutex: Arc::new(Mutex::new(tool_db_access_tuple)),
            main_page: String::from_utf8_lossy(
                &fs::read(main_page_path).expect("Could not read index.html file form disk"),
            )
            .parse()
            .expect("Parsing index.html failed"),
        };

        match config["mysql"].as_array() {
            Some(up_list) => {
                up_list.iter().for_each(|up| {
                    let user = up[0]
                        .as_str()
                        .expect("Parsing user from mysql array in config failed")
                        .to_string();
                    let pass = up[1]
                        .as_str()
                        .expect("Parsing pass from mysql array in config failed")
                        .to_string();
                    let connections = up[2].as_u64().unwrap_or(5);
                    for _connection_num in 1..connections {
                        let tuple = (user.to_owned(), pass.to_owned());
                        ret.db_pool.push(Arc::new(Mutex::new(tuple)));
                    }
                    // Ignore toolname up[3]
                });
            }
            None => {
                for _x in 1..MAX_CONCURRENT_DB_CONNECTIONS {
                    let tuple = (
                        config["user"]
                            .as_str()
                            .expect("No user key in config file")
                            .to_string(),
                        config["password"]
                            .as_str()
                            .expect("No password key in config file")
                            .to_string(),
                    );
                    ret.db_pool.push(Arc::new(Mutex::new(tuple)));
                }
            }
        }
        if ret.db_pool.is_empty() {
            panic!("No database access config available");
        }
        ret
    }

    pub fn get_main_page(&self, interface_language: String) -> String {
        let direction = if self.is_language_rtl(&interface_language) {
            "rtl"
        } else {
            "ltr"
        };
        let h = format!(
            "<html dir='{}' lang='{}'>",
            direction,
            interface_language.replace("'", "")
        );
        self.main_page.replace("<html>", &h).to_string()
    }

    fn get_db_server_group(&self) -> &str {
        match self.config["dbservergroup"].as_str() {
            Some(s) => s,
            None => ".web.db.svc.eqiad.wmflabs", // ".analytics.db.svc.eqiad.wmflabs"
        }
    }

    /// Returns the server and database name for the wiki, as a tuple
    pub fn db_host_and_schema_for_wiki(&self, wiki: &String) -> Result<(String, String), String> {
        // TESTING
        // ssh magnus@tools-login.wmflabs.org -L 3307:wikidatawiki.web.db.svc.eqiad.wmflabs:3306 -N
        lazy_static! {
            static ref REMOVE_WIKI: Regex = Regex::new(r"wiki$")
                .expect("AppState::get_url_for_wiki_from_site: Regex is invalid");
        }

        let wiki = match wiki.as_str() {
            "be-taraskwiki" | "be-x-oldwiki" | "be_taraskwiki" | "be_x_oldwiki" => "be_x_oldwiki",
            other => other,
        }
        .to_string();

        let host = match self.config["host"].as_str() {
            Some("127.0.0.1") => "127.0.0.1".to_string(),
            Some(_host) => wiki.to_owned() + self.get_db_server_group(),
            None => panic!("No host in config file"),
        };
        let schema = wiki.to_string() + "_p";
        Ok((host, schema))
    }

    /// Returns the server and database name for the tool db, as a tuple
    pub fn db_host_and_schema_for_tool_db(&self) -> (String, String) {
        // TESTING
        // ssh magnus@tools-login.wmflabs.org -L 3308:tools-db:3306 -N
        let host = self.config["host"]
            .as_str()
            .expect("No host key in config file")
            .to_string();
        let schema = self.config["schema"]
            .as_str()
            .expect("No schema key in config file")
            .to_string();
        (host, schema)
    }

    /// Returns a random mutex. The mutex value itself contains a user name and password for DB login!
    pub fn get_db_mutex(&self) -> &Arc<Mutex<DbUserPass>> {
        let ten_millis = time::Duration::from_millis(500); // 0.5 sec
        let mut countdown: usize = self.db_pool.len() * 2;
        loop {
            // Slow down if free mutex proves hard to find
            countdown -= 1;
            if countdown == 0 {
                countdown = self.db_pool.len() * 2;
                thread::sleep(ten_millis);
            }
            let ret = match self.db_pool.choose(&mut rand::thread_rng()) {
                Some(db) => db,
                None => continue,
            };
            // make sure mutex is not poisoned
            if ret.is_poisoned() {
                continue;
            }
            // make sure mutex is available
            match ret.try_lock() {
                Ok(_) => return &ret,
                _ => continue,
            }
        }
    }

    fn set_group_concat_max_len(&self, wiki: &String, conn: &mut my::Conn) -> Result<(), String> {
        if wiki != "commonswiki" {
            return Ok(()); // Only needed for commonswiki, in platform::process_files
        }
        let sql: SQLtuple = (
            "SET SESSION group_concat_max_len = 1000000000".to_string(),
            vec![],
        );
        conn.prep_exec(&sql.0, &sql.1).map_err(|e| {
            format!(
                "AppState::set_group_concat_max_len: SQL query error: {:?}",
                e
            )
        })?;
        Ok(())
    }

    pub fn get_wiki_db_connection(
        &self,
        db_user_pass: &DbUserPass,
        wiki: &String,
    ) -> Result<my::Conn, String> {
        let mut loops_left = MYSQL_MAX_CONNECTION_ATTEMPTS;
        let mut milliseconds = MYSQL_CONNECTION_INITIAL_DELAY_MS;
        let (host, schema) = self.db_host_and_schema_for_wiki(wiki)?;
        let (user, pass) = db_user_pass;
        loop {
            let mut builder = my::OptsBuilder::new();
            builder
                .ip_or_hostname(Some(host.to_owned()))
                .db_name(Some(schema.to_owned()))
                .user(Some(user))
                .pass(Some(pass));
            builder.tcp_port(self.config["db_port"].as_u64().unwrap_or(3306) as u16);

            match my::Conn::new(builder) {
                Ok(mut con) => {
                    self.set_group_concat_max_len(wiki, &mut con)?;
                    return Ok(con);
                }
                Err(e) => {
                    if loops_left == 0 {
                        println!("CONNECTION ERROR: {:?}\nfor user {}", e, &user);
                        break;
                    }
                    loops_left -= 1;
                    let sleep_ms = time::Duration::from_millis(milliseconds);
                    milliseconds *= 2;
                    if milliseconds > MYSQL_CONNECTION_MAX_DELAY_MS {
                        milliseconds = MYSQL_CONNECTION_MAX_DELAY_MS;
                    }
                    thread::sleep(sleep_ms);
                }
            }
        }
        Err(format!(
            "Could not connect to database replica for '{}' on '{}'/'{}' after {} attempts",
            &wiki, &host, &schema, MYSQL_MAX_CONNECTION_ATTEMPTS
        ))
    }

    pub fn render_error(&self, error: String, form_parameters: &FormParameters) -> MyResponse {
        match form_parameters.params.get("format").map(|s| s.as_str()) {
            Some("") | Some("html") => {
                let output = format!(
                    "<div class='alert alert-danger' role='alert'>{}</div>",
                    &error
                );
                let interface_language = form_parameters
                    .params
                    .get("interface_language")
                    .map(|s| s.to_string())
                    .unwrap_or("en".to_string());
                let html = self
                    .get_main_page(interface_language.to_string())
                    .to_owned();
                let html = html.replace("<!--querystring-->", form_parameters.to_string().as_str());
                let html = &html.replace("<!--output-->", &output);
                MyResponse {
                    s: html.to_string(),
                    content_type: ContentType::HTML,
                }
            }
            Some("json") => {
                let value = json!({ "error": error });
                self.output_json(&value, form_parameters.params.get("callback"))
            }
            _ => MyResponse {
                s: error.to_string(),
                content_type: ContentType::Plain,
            },
        }
    }

    pub fn output_json(&self, value: &Value, callback: Option<&String>) -> MyResponse {
        match callback {
            Some(callback) => {
                let mut text = callback.to_owned();
                text += "(";
                text += &::serde_json::to_string(&value)
                    .expect("app_state::output_json can't stringify JSON [1]");
                text += ")";
                MyResponse {
                    s: text,
                    content_type: ContentType::JSONP,
                }
            }
            None => MyResponse {
                s: ::serde_json::to_string(&value)
                    .expect("app_state::output_json can't stringify JSON [2]"),
                content_type: ContentType::JSON,
            },
        }
    }

    pub fn get_api_for_wiki(&self, wiki: String) -> Result<Api, String> {
        // TODO cache url and/or api object?
        let url = self.get_server_url_for_wiki(&wiki)? + "/w/api.php";
        match Api::new(&url) {
            Ok(api) => Ok(api),
            Err(e) => Err(format!("{:?}", e)),
        }
    }

    fn get_value_from_site_matrix_entry(
        &self,
        value: &String,
        site: &Value,
        key_match: &str,
        key_return: &str,
    ) -> Option<String> {
        if site["closed"].as_str().is_some() {
            return None;
        }
        if site["private"].as_str().is_some() {
            return None;
        }
        match site[key_match].as_str() {
            Some(site_url) => {
                if value == site_url {
                    match site[key_return].as_str() {
                        Some(url) => Some(url.to_string()),
                        None => None,
                    }
                } else {
                    None
                }
            }
            None => None,
        }
    }

    fn get_wiki_for_server_url_from_site(&self, url: &String, site: &Value) -> Option<String> {
        self.get_value_from_site_matrix_entry(url, site, "url", "dbname")
    }

    fn get_url_for_wiki_from_site(&self, wiki: &String, site: &Value) -> Option<String> {
        self.get_value_from_site_matrix_entry(wiki, site, "dbname", "url")
    }

    pub fn is_language_rtl(&self, language: &str) -> bool {
        self.site_matrix["sitematrix"]
            .as_object()
            .expect("AppState::get_wiki_for_server_url: sitematrix not an object")
            .iter()
            .any(
                |(_id, data)| match (data["code"].as_str(), data["dir"].as_str()) {
                    (Some(lang), Some("rtl")) => lang == language,
                    _ => false,
                },
            )
    }

    pub fn get_wiki_for_server_url(&self, url: &String) -> Option<String> {
        self.site_matrix["sitematrix"]
            .as_object()
            .expect("AppState::get_wiki_for_server_url: sitematrix not an object")
            .iter()
            .filter_map(|(id, data)| match id.as_str() {
                "count" => None,
                "specials" => data
                    .as_array()
                    .expect("AppState::get_wiki_for_server_url: 'specials' is not an array")
                    .iter()
                    .filter_map(|site| self.get_wiki_for_server_url_from_site(url, site))
                    .next(),
                _other => match data["site"].as_array() {
                    Some(sites) => sites
                        .iter()
                        .filter_map(|site| self.get_wiki_for_server_url_from_site(url, site))
                        .next(),
                    None => None,
                },
            })
            .next()
    }

    pub fn get_server_url_for_wiki(&self, wiki: &String) -> Result<String, String> {
        match wiki.replace("_", "-").as_str() {
            "be-taraskwiki" | "be-x-oldwiki" => {
                return Ok("https://be-tarask.wikipedia.org".to_string())
            }
            _ => {}
        }
        self.site_matrix["sitematrix"]
            .as_object()
            .expect("AppState::get_server_url_for_wiki: sitematrix not an object")
            .iter()
            .filter_map(|(id, data)| match id.as_str() {
                "count" => None,
                "specials" => data
                    .as_array()
                    .expect("AppState::get_server_url_for_wiki: 'specials' is not an array")
                    .iter()
                    .filter_map(|site| self.get_url_for_wiki_from_site(wiki, site))
                    .next(),
                _other => match data["site"].as_array() {
                    Some(sites) => sites
                        .iter()
                        .filter_map(|site| self.get_url_for_wiki_from_site(wiki, site))
                        .next(),
                    None => None,
                },
            })
            .next()
            .ok_or(format!(
                "AppState::get_server_url_for_wiki: Cannot find server for wiki '{}'",
                &wiki
            ))
    }

    pub fn get_tool_db_connection(
        &self,
        tool_db_user_pass: DbUserPass,
    ) -> Result<my::Conn, String> {
        let (host, schema) = self.db_host_and_schema_for_tool_db();
        let (user, pass) = tool_db_user_pass.clone();
        let mut builder = my::OptsBuilder::new();
        builder
            .ip_or_hostname(Some(host.to_owned()))
            .db_name(Some(schema))
            .user(Some(user))
            .pass(Some(pass));
        let port: u16 = match self.config["host"].as_str() {
            Some("127.0.0.1") => 3308,
            Some(_host) => self.config["db_port"].as_u64().unwrap_or(3306) as u16,
            None => 3306, // Fallback
        };
        builder.tcp_port(port);

        match my::Conn::new(builder) {
            Ok(conn) => Ok(conn),
            Err(e) => Err(format!(
                "AppState::get_tool_db_connection can't get DB connection to {}:{} : '{}'",
                &host, port, &e
            )),
        }
    }

    pub fn get_tool_db_user_pass(&self) -> &Arc<Mutex<DbUserPass>> {
        &self.tool_db_mutex
    }

    pub fn get_query_from_psid(&self, psid: &String) -> Result<String, String> {
        let tool_db_user_pass = self.tool_db_mutex.lock().map_err(|e| format!("{:?}", e))?;
        let mut conn = self.get_tool_db_connection(tool_db_user_pass.clone())?;

        let psid = match psid.parse::<usize>() {
            Ok(psid) => psid,
            Err(e) => return Err(format!("{:?}", e)),
        };
        let sql = format!("SELECT querystring FROM query WHERE id={}", psid);
        let result = match conn.prep_exec(sql, ()) {
            Ok(r) => r,
            Err(e) => {
                return Err(format!(
                    "AppState::get_query_from_psid query error: {:?}",
                    e
                ))
            }
        };
        let ret = result
            .filter_map(|row_result| row_result.ok())
            .filter_map(|row| my::from_row_opt::<Vec<u8>>(row).ok())
            .next();
        match ret {
            Some(ret) => Ok(String::from_utf8_lossy(&ret).into_owned()),
            None => Err("No such PSID in the database".to_string()),
        }
    }

    pub fn log_query_start(&self, query_string: &String) -> Result<u64, String> {
        let tool_db_user_pass = self.tool_db_mutex.lock().map_err(|e| format!("{:?}", e))?;
        let mut conn = self.get_tool_db_connection(tool_db_user_pass.clone())?;
        let utc: DateTime<Utc> = Utc::now();
        let now = utc.format("%Y-%m-%d %H:%M:%S").to_string();
        let sql = (
            "INSERT INTO `started_queries` (querystring,created,process_id) VALUES (?,?,?)"
                .to_string(),
            vec![
                query_string.to_owned(),
                now,
                format!("{}", std::process::id()),
            ],
        );
        let ret = match conn.prep_exec(sql.0, sql.1) {
            Ok(r) => Ok(r.last_insert_id()),
            Err(e) => Err(format!(
                "AppState::get_new_psid_for_query query error: {:?}",
                e
            )),
        };
        ret
    }

    pub fn log_query_end(&self, query_id: u64) {
        let tool_db_user_pass = match self.tool_db_mutex.lock() {
            Ok(x) => x,
            Err(_e) => return,
        };
        let mut conn = match self.get_tool_db_connection(tool_db_user_pass.clone()) {
            Ok(conn) => conn,
            _ => return,
        };
        let sql = (
            "DELETE FROM `started_queries` WHERE id=?".to_string(),
            vec![format!("{}", query_id)],
        );
        match conn.prep_exec(sql.0, sql.1) {
            Ok(_r) => {}
            Err(_e) => {}
        };
    }

    pub fn get_or_create_psid_for_query(&self, query_string: &String) -> Result<u64, String> {
        let tool_db_user_pass = self.tool_db_mutex.lock().map_err(|e| format!("{:?}", e))?;
        let mut conn = self.get_tool_db_connection(tool_db_user_pass.clone())?;

        // Check for existing entry
        let sql = (
            "SELECT id FROM query WHERE querystring=? LIMIT 1".to_string(),
            vec![query_string.to_owned()],
        );
        match conn.prep_exec(sql.0, sql.1) {
            Ok(result) => {
                let ret = result
                    .filter_map(|row_result| row_result.ok())
                    .filter_map(|row| my::from_row_opt::<u64>(row).ok())
                    .next();
                match ret {
                    Some(ret) => return Ok(ret),
                    None => {}
                };
            }
            Err(_) => {}
        }

        // Create new entry
        let utc: DateTime<Utc> = Utc::now();
        let now = utc.format("%Y-%m-%d %H:%M:%S").to_string();
        let sql = (
            "INSERT IGNORE INTO `query` (querystring,created) VALUES (?,?)".to_string(),
            vec![query_string.to_owned(), now],
        );
        let ret = match conn.prep_exec(sql.0, sql.1) {
            Ok(r) => Ok(r.last_insert_id()),
            Err(e) => Err(format!(
                "AppState::get_new_psid_for_query query error: {:?}",
                e
            )),
        };
        ret
    }

    fn load_site_matrix() -> Value {
        let api =
            Api::new("https://www.wikidata.org/w/api.php").expect("Can't talk to Wikidata API");
        let params: HashMap<String, String> = vec![("action", "sitematrix")]
            .par_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        api.get_query_api_json(&params)
            .expect("Can't run action=sitematrix on Wikidata API")
    }

    pub fn try_shutdown(&self) {
        if self.is_shutting_down() && *self.threads_running.read().unwrap() == 0 {
            ::std::process::exit(0);
        }
    }

    pub fn modify_threads_running(&self, diff: i64) {
        *self.threads_running.write().unwrap() += diff;
        self.try_shutdown()
    }

    pub fn is_shutting_down(&self) -> bool {
        *self.shutting_down.read().unwrap()
    }

    pub fn shut_down(&self) {
        *self.shutting_down.write().unwrap() = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    //use crate::app_state::AppState;
    use serde_json::Value;
    use std::env;
    use std::fs::File;

    fn get_new_state() -> Arc<AppState> {
        let basedir = env::current_dir()
            .expect("Can't get CWD")
            .to_str()
            .unwrap()
            .to_string();
        let path = basedir.to_owned() + "/config.json";
        let file = File::open(path).expect("Can not open config file");
        let petscan_config: Value =
            serde_json::from_reader(file).expect("Can not parse JSON from config file");
        Arc::new(AppState::new_from_config(&petscan_config))
    }

    fn get_state() -> Arc<AppState> {
        lazy_static! {
            static ref STATE: Arc<AppState> = get_new_state();
        }
        STATE.clone()
    }

    #[test]
    fn test_get_wiki_for_server_url() {
        let state = get_state();
        assert_eq!(
            state.get_wiki_for_server_url(&"https://am.wiktionary.org".to_string()),
            Some("amwiktionary".to_string())
        );
        assert_eq!(
            state.get_wiki_for_server_url(&"https://outreach.wikimedia.org".to_string()),
            Some("outreachwiki".to_string())
        );
    }

    #[test]
    fn test_db_host_and_schema_for_wiki() {
        let state = get_state();
        assert_eq!(
            "enwiki_p".to_string(),
            state
                .db_host_and_schema_for_wiki(&"enwiki".to_string())
                .unwrap()
                .1
        );
        assert_eq!(
            "be_x_oldwiki_p".to_string(),
            state
                .db_host_and_schema_for_wiki(&"be-taraskwiki".to_string())
                .unwrap()
                .1
        );
    }

    #[test]
    fn is_language_rtl() {
        let state = get_state();
        assert!(!state.is_language_rtl("en"));
        assert!(state.is_language_rtl("ar"));
        assert!(!state.is_language_rtl("de"));
        assert!(state.is_language_rtl("he"));
    }
}
