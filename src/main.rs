use anyhow::{Context, Result};
use futures_util::{Stream, StreamExt};
use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use log::{info, warn, debug};
use reqwest::{
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, RETRY_AFTER},
    Response,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use futures_util::pin_mut;

/// Program to delete your messages from a slack conversation.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Your user ID
    user_id: String,

    /// ID of the conversation that you want to be cleaned
    conversation_id: String,
}

struct SlackClient {
    token: String,
}

/// General Slack API response
trait SlackApiResponse {
    fn is_ok(&self) -> bool;
    fn get_error(&self) -> Option<&String>;
}

#[derive(Serialize, Deserialize, Debug)]
struct HistoryResponse {
    ok: bool,
    messages: Option<Vec<SlackMessage>>,
    has_more: Option<bool>,
    response_metadata: Option<ResponseMetadata>,
    error: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
struct DeleteResponse {
    ok: bool,
    error: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
struct RepliesResponse {
    ok: bool,
    messages: Option<Vec<SlackMessage>>,
    has_more: Option<bool>,
    response_metadata: Option<ResponseMetadata>,
    error: Option<String>,
}

impl SlackApiResponse for HistoryResponse {
    fn is_ok(&self) -> bool {
        self.ok
    }
    fn get_error(&self) -> Option<&String> {
        self.error.as_ref()
    }
}

impl SlackApiResponse for DeleteResponse {
    fn is_ok(&self) -> bool {
        self.ok
    }
    fn get_error(&self) -> Option<&String> {
        self.error.as_ref()
    }
}

impl SlackApiResponse for RepliesResponse {
    fn is_ok(&self) -> bool {
        self.ok
    }
    fn get_error(&self) -> Option<&String> {
        self.error.as_ref()
    }
}

#[derive(Serialize, Deserialize, Debug)]
struct SlackMessage {
    r#type: String,
    user: Option<String>,
    text: String,
    ts: String,
    thread_ts: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
struct ResponseMetadata {
    next_cursor: Option<String>,
}

async fn get<T>(slack_client: &SlackClient, route: &str, data: HashMap<&str, String>) -> Result<T>
where
    T: DeserializeOwned + SlackApiResponse,
{
    let client = reqwest::Client::new();

    loop {

        let url = reqwest::Url::parse_with_params(&format!("https://slack.com/api/{}", route), &data)?;

        let response = client.get(url)
            .header(AUTHORIZATION, format!("Bearer {}", slack_client.token))
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(ACCEPT, "application/json")
            .send()
            .await?;

        let retry_after = get_retry_after(&response);

        let parsed: T = response.json::<T>().await?;

        if !parsed.is_ok() {
            match parsed.get_error() {
                Some(error_message) => {
                    if error_message == "ratelimited" {
                        info!("Rate limited, retrying after {} seconds.", retry_after);
                        tokio::time::sleep(tokio::time::Duration::from_secs(retry_after)).await;
                    } else {
                        warn!("Error {} in {}", error_message, route);
                        return Err(anyhow::anyhow!(error_message.clone()));
                    }
                }
                None => {
                    return Err(anyhow::anyhow!("Unknown error"));
                }
            }
        } else {
            return Ok(parsed);
        }
    }
}

async fn post<T>(slack_client: &SlackClient, route: &str, data: HashMap<&str, String>) -> Result<T>
where
    T: DeserializeOwned + SlackApiResponse,
{
    let client = reqwest::Client::new();

    loop {
        let response = client
            .post(format!("https://slack.com/api/{}", route))
            .header(AUTHORIZATION, format!("Bearer {}", slack_client.token))
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .json(&data)
            .send()
            .await?;
        let retry_after = get_retry_after(&response);

        let parsed = response.json::<T>().await?;

        if !parsed.is_ok() {
            match parsed.get_error() {
                Some(error_message) => {
                    if error_message == "ratelimited" {
                        info!("Rate limited, retrying after {} seconds.", retry_after);
                        tokio::time::sleep(tokio::time::Duration::from_secs(retry_after)).await;
                    } else {
                        warn!("Error {} in {}", error_message, route);
                        return Err(anyhow::anyhow!(error_message.clone()));
                    }
                }
                None => {
                    return Err(anyhow::anyhow!("Unknown error"));
                }
            }
        } else {
            return Ok(parsed);
        }
    }
}

fn get_retry_after(response: &Response) -> u64 {
    let default = 1;

    match response.headers().get(RETRY_AFTER) {
        Some(value) => match value.to_str() {
            Ok(value) => match value.parse::<u64>() {
                Ok(number) => number,
                Err(_) => default,
            },
            Err(_) => default,
        },
        None => default,
    }
}

/// Delete given message. Doesn't care if the app has permission for the
/// message. The calling function will ensure that to happen.
async fn delete_message(slack_client: &SlackClient, conversation_id: &str, ts: &str) -> Result<()> {
    let map = HashMap::from([
        ("channel", conversation_id.to_string()),
        ("ts", ts.to_string()),
    ]);

    let _response: DeleteResponse = post(slack_client, "/chat.delete", map).await?;
    Ok(())
}

/// Get history of a conversation id. As of now only return 200 results. This
/// will get converted to a stream later for proper functioning.
async fn get_history(slack_client: &SlackClient, conversation_id: &str, cursor: Option<&str>) -> Result<HistoryResponse> {
    let mut map = HashMap::from([
        ("channel", conversation_id.to_string()),
        ("limit", "200".to_string()),
    ]);

    if let Some(cursor) = cursor {
        map.insert("cursor", cursor.to_string());
    }

    post(slack_client, "/conversations.history", map).await
}

async fn get_replies(slack_client: &SlackClient, conversation_id: &str, thread_ts: &str) -> Result<RepliesResponse> {
    let map = HashMap::from([
        ("channel", conversation_id.to_string()),
        ("ts", thread_ts.to_string()),
    ]);

    get(slack_client, "/conversations.replies", map).await
}

fn get_messages<'a>(
    slack_client: &'a SlackClient,
    conversation_id: &'a str
) -> impl Stream<Item = SlackMessage> + 'a {
    async_stream::stream! {
        let mut cursor = None;

        loop {
            let history = get_history(slack_client, conversation_id, cursor.as_deref()).await;

            match history {
                Ok(history) => {
                    if let Some(messages) = history.messages {
                        for message in messages {
                            match message.thread_ts.clone() {
                                Some(thread_ts) => {
                                    let replies = get_replies(slack_client, conversation_id, &thread_ts).await;

                                    match replies {
                                        Ok(replies) => {
                                            for reply in replies.messages.unwrap_or_default() {
                                                yield reply;
                                            }
                                        }
                                        Err(_e) => {}
                                    }
                                }
                                None => {}
                            }
                            yield message;
                        }
                    }

                    cursor = history.response_metadata.and_then(|meta| meta.next_cursor);
                    if cursor.is_none() {
                        break;
                    }
                }
                Err(_e) => {
                    break;
                }
            }
        }
    }
}


#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let args = Cli::parse();

    let token = env::var("SLACK_USER_TOKEN").context("SLACK_USER_TOKEN not found")?;
    let slack_client = SlackClient { token };

    let mut n_messages = 0;
    let mut n_deleted = 0;

    let message_stream = get_messages(&slack_client, &args.conversation_id);
    pin_mut!(message_stream);

    let bar = ProgressBar::new_spinner();
    bar.set_style(ProgressStyle::default_spinner()
        .template("{spinner:.green} [{elapsed}] [{pos}/{len}] {msg}")
        .unwrap()
        .progress_chars("#>-"));

    while let Some(message) = message_stream.next().await {
        match message.user {
            Some(user_id) => {
                if user_id == args.user_id {
                    let _ = delete_message(&slack_client, &args.conversation_id, &message.ts).await;
                    n_deleted += 1;
                }
            }
            None => {}
        }
        n_messages += 1;

        bar.inc(1);
    }

    bar.finish();
    println!("Processed {} messages, deleted {}.", n_messages, n_deleted);

    Ok(())
}
