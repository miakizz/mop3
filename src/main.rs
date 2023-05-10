use std::{
    io::{prelude::*, BufReader},
    net::{TcpListener, TcpStream},
    thread,
    //fs
};

use chrono::naive::NaiveDateTime;
use chrono::offset::Utc;
use chrono::DateTime;
use clap::Parser;
use deunicode::deunicode;
use html2text::from_read;
use mail_builder::*;
use mail_parser::*;
use onig::*;
use reqwest::blocking::{multipart::Form, multipart::Part, Client};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use string_concat::*;

#[derive(Debug)]
struct Cred {
    username: String,
    password: String,
}

#[derive(Serialize)]
struct Status {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    in_reply_to_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    media_ids: Vec<String>,
}

/// Data returned from verify credentials call
///
/// <https://docs.joinmastodon.org/methods/accounts/#verify_credentials>
#[derive(Deserialize)]
struct CredentialAccount {
    display_name: String,
    username: String,
}

#[derive(Debug)]
struct Attachment {
    filename: String,
    content_type: String,
    data: Vec<u8>,
}

#[allow(unused)]
enum POPCommand {
    Quit,
    Stat,
    List(u32),
    Retr(u32),
    Dele(u32),
    Noop,
    Rset,
    Top { msg: u32, n: u32 },
    Uidl(u32),
    User(String),
    Pass(String),
    Apop,
    Disconnect,
    Capa,
    Auth,
}
enum SMTPCommand {
    Mailfrom(String),
    RcptTo(String),
    Data(String),
    Rset,
    Noop,
    Quit,
    Disconnect,
    Helo,
    Ehlo,
}

//This basically converts the Err to Option, without having to borrow the stream
//A little hacky?
macro_rules! send_str {
    ($stream:expr,$msg:expr) => {
        $stream.write_all($msg.as_bytes()).ok()
    };
}

#[derive(Parser, Debug)]
#[command(name = "MOP3")]
#[command(author = "Nathan Kiesman. <nkizz@tacobelllabs.net>")]
#[command(version = "0.1")]
#[command(about = "Mastodon to POP3 gateway", long_about = None)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// If set, mastodon account to connect to, ex. user@example.com. Otherwise, uses username from POP3/SMTP connection
    #[arg(long)]
    account: Option<String>,
    /// If set, mastodon token to connect with. Otherwise, uses password from POP3 connection. Required for SMTP server
    #[arg(long)]
    token: Option<String>,
    /// Address to listen on, defaults to localhost
    #[arg(long)]
    address: Option<String>,
    /// POP3 listening port, defaults to 110
    #[arg(long)]
    pop3port: Option<u16>,
    /// SMTP listening port, defaults to 25
    #[arg(long)]
    smtpport: Option<u16>,
    /// Only send ASCII to clients, gracefully converts unicode
    #[arg(long)]
    ascii: bool,
    /// Disables SMTP, posts can only be received, not sent
    #[arg(long)]
    nosmtp: bool,
    /// Enables SMTP, ignored since it is now the default
    #[arg(long,hide=true)]
    smtp: bool,
    /// Enables adding images as binary attachments, don't use with --inline
    #[arg(long)]
    attachment: bool,
    /// Enables inline images, don't use with --attachment
    #[arg(long)]
    inline: bool,
    /// Disables HTML to text conversion, makes links look better if you're using a client that supports HTML
    #[arg(long)]
    html: bool,
}

fn main() {
    let args = Args::parse();
    if !args.nosmtp {
        if args.token.is_none() {
            println!("Error: Must provide token to use SMTP server,");
            println!("since I was too lazy to implement SMTP auth.");
            println!("If this is an issue, please let me know.");
            return;
        }
        thread::spawn(smtp_setup);
    }
    //Most recent ID fetched, passed to API call to reduce server load
    let mut recent = "".to_string();
    let account = (
        args.address.as_deref().unwrap_or("127.0.0.1"),
        args.pop3port.unwrap_or(110),
    );
    println!("Listening on {:?}", account);
    loop {
        let listener = TcpListener::bind(account).unwrap();
        for stream in listener.incoming() {
            let stream = stream.unwrap();
            if let Some(new_recent) = handle_pop_connection(&args, stream, recent.clone()) {
                recent = new_recent;
            };
        }
    }
}
fn smtp_setup() {
    let args = Args::parse();
    let smtp_addr = (
        args.address.as_deref().unwrap_or("127.0.0.1"),
        args.smtpport.unwrap_or(25),
    );
    println!("Listening for SMTP on {:?}", smtp_addr);
    loop {
        let smtp_listener = TcpListener::bind(smtp_addr).unwrap();
        for stream in smtp_listener.incoming() {
            handle_smtp_connection(stream.unwrap(), &args);
        }
    }
}

fn handle_pop_connection(
    args: &Args,
    mut stream: TcpStream,
    mut recent_id: String,
) -> Option<String> {
    stream
        .write_all("+OK MOP3 ready\r\n".as_bytes())
        .expect("Couldn't send welcome message");

    let new_cred_res = get_login(&mut stream);
    //Make sure we didn't drop the connection
    let mut new_cred = match new_cred_res {
        Some(cred) => cred,
        None => return None,
    };
    //If credentials have been passed in on the CLI, use them
    if args.account.as_deref().is_some() {
        new_cred.username = args.account.as_deref()?.to_string();
    }
    if args.token.as_deref().is_some() {
        new_cred.password = args.token.as_deref()?.to_string();
    }
    let (account_domain, account_url) = strip_cred(&new_cred.username);

    let client = Client::new();

    //Verify account and get user's display name
    let account: CredentialAccount = client
        .get(format!("{account_url}/api/v1/accounts/verify_credentials"))
        .header("Authorization", "Bearer ".to_owned() + &new_cred.password)
        .send()
        .expect("Could not verify credentials")
        .json()
        .unwrap();

    let account_addr = format!("{}@{}", account.username, account_domain);

    //Get timeline
    let since_id = (!recent_id.is_empty())
        .then(|| format!("&since_id={recent_id}"))
        .unwrap_or_default();
    let mut timeline_str = client
        .get(format!(
            "{account_url}/api/v1/timelines/home?limit=40{since_id}"
        ))
        .header("Authorization", "Bearer ".to_owned() + &new_cred.password)
        .send()
        .expect("Could not retrieve timeline")
        .text()
        .unwrap();
    if args.ascii {
        timeline_str = deunicode(&timeline_str);
    }
    //println!("{}", timeline_str);
    let timeline: Vec<Value> =
        serde_json::from_str(&timeline_str).expect("Server sent malformed JSON");

    //Total size of all emails, needs to be reported back
    let mut post_size = 0;
    let mut emails: Vec<String> = vec![];
    for post in &timeline {
        println!("{}", get_str(&post["created_at"]));
        //If this is a reblog, get text & images from the reblog
        let (mut content, media_vec, subject) = if post["reblog"] != Value::Null {
            (
                get_str(&post["reblog"]["content"]).to_string(),
                &post["reblog"]["media_attachments"],
                "Boost",
            )
        } else {
            (
                get_str(&post["content"]).to_string(),
                &post["media_attachments"],
                "Post",
            )
        };
        //De-HTML-ify content if requested
        if !args.html {
            content = from_read(content.as_bytes(), 78).replace('\n', "\r\n");
        }
        //Get URLs of any media, and either append them as text, or download images into a Vec
        let media_urls = media_vec
            .as_array()
            .expect("Server sent malformed JSON (no media array)");
        let mut attachments = Vec::new();
        if args.attachment || args.inline {
            for media in media_urls {
                //Extract info from the JSON response and fetch image
                let img = client
                    .get(get_str(&media["url"]))
                    .send()
                    .expect("Couldn't get image");
                let filename = get_str(&media["url"])
                    .split('/')
                    .last()
                    .unwrap()
                    .to_string();
                let mime = img
                    .headers()
                    .get("Content-Type")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string();
                let img_data = img.bytes().unwrap().clone().to_owned();
                attachments.push(Attachment {
                    filename,
                    content_type: mime,
                    data: img_data.to_vec(),
                });
            }
        } else {
            for media in media_vec
                .as_array()
                .expect("Server sent malformed JSON (no media array)")
            {
                content = string_concat!(content, "\r\n", get_str(&media["url"]));
            }
        }
        //oh lawd he comin
        let mut message = MessageBuilder::new()
            .from((
                get_str(&post["account"]["display_name"]),
                get_str(&post["account"]["acct"]),
            ))
            .to((account.display_name.clone(), account_addr.clone()))
            .subject(subject)
            //Fun fact: this line of code is 181 characters long
            .date(
                DateTime::<Utc>::from_utc(
                    NaiveDateTime::parse_from_str(
                        get_str(&post["created_at"]),
                        "%Y-%m-%dT%H:%M:%S%.3fZ",
                    )
                    .expect("Server sent unexpected time format"),
                    Utc,
                )
                .timestamp(),
            )
            .message_id(string_concat!(get_str(&post["id"]), "@", account_domain));
        if args.html {
            message = message.html_body(content);
        } else {
            message = message.text_body(content);
        }
        if post["in_reply_to_id"] != Value::Null {
            message = message.in_reply_to(string_concat!(
                get_str(&post["in_reply_to_id"]),
                "@",
                account_domain
            ));
        }
        if args.attachment {
            for attachment in attachments {
                message = message.binary_attachment(
                    attachment.content_type,
                    attachment.filename,
                    attachment.data,
                );
            }
        } else if args.inline {
            for attachment in attachments {
                message = message.binary_inline(
                    attachment.content_type,
                    attachment.filename,
                    attachment.data,
                );
            }
        }
        let message = string_concat!(message.write_to_string().unwrap(), "\r\n");
        //std::fs::write("debug", &message).expect("Could not write debug file");
        post_size += message.len();
        emails.push(message);
    }
    send_str!(stream, "+OK MOP3 READY, MESSAGES FETCHED\r\n")?;
    //process commands as we get them
    loop {
        //what if we kissed in The TRANSACTION State
        match get_pop_command(&mut stream) {
            POPCommand::List(index) => {
                let i = index as usize;
                if i != 0 {
                    if i > emails.len() {
                        send_str!(stream, "-ERR no such message\r\n")?;
                    } else {
                        send_str!(stream, &format!("+OK {} {}\r\n", i, emails[i - 1].len()))?;
                    }
                } else {
                    send_str!(
                        stream,
                        &format!("+OK {} messages ({} octets)\r\n", emails.len(), post_size)
                    )?;
                    for (i, msg) in emails.iter().enumerate() {
                        send_str!(stream, &format!("{} {}\r\n", i + 1, msg.len()))?;
                    }
                    send_str!(stream, ".\r\n")?;
                }
            }

            POPCommand::Retr(i) => {
                let ind = (i - 1) as usize;
                if ind >= emails.len() {
                    send_str!(stream, "-ERR no such message\r\n")?;
                } else {
                    send_str!(stream, &format!("+OK {} octets\r\n", emails[ind].len()))?;
                    send_str!(stream, &emails[ind])?;
                    send_str!(stream, ".\r\n")?;
                    recent_id = get_str(&timeline[0]["id"]).to_string();
                }
            }

            POPCommand::Stat => {
                send_str!(stream, &format!("+OK {} {}\r\n", emails.len(), post_size))?
            }
            POPCommand::Uidl(ind) => {
                let i = ind as usize;
                if i != 0 {
                    if i > emails.len() {
                        send_str!(stream, "-ERR no such message\r\n")?;
                    } else {
                        send_str!(
                            stream,
                            &string_concat!(
                                "+OK ",
                                ind.to_string(),
                                " ",
                                get_str(&timeline[i - 1]["id"]),
                                "@",
                                account_domain,
                                "\r\n"
                            )
                        )?;
                    }
                } else {
                    send_str!(stream, "+OK\r\n")?;
                    for (i, msg) in timeline.iter().enumerate() {
                        send_str!(
                            stream,
                            &format!("{} {}@{}\r\n", i + 1, get_str(&msg["id"]), account_domain)
                        )?;
                    }
                    send_str!(stream, ".\r\n")?;
                }
            }
            POPCommand::Top { msg, mut n } => {
                //This is basically RETR
                let ind = (msg - 1) as usize;
                if ind >= emails.len() {
                    send_str!(stream, "-ERR no such message\r\n")?;
                } else {
                    let mut partial = "".to_string();
                    let lines = emails[ind].lines();
                    let mut msg_flag = false;
                    for line in lines {
                        //This is a terribly inefficent way to do this
                        partial = string_concat!(partial, line, "\r\n");
                        if msg_flag {
                            if n == 0 {
                                break;
                            }
                            n -= 1;
                        }
                        if line == "" {
                            msg_flag = true;
                        }
                    }
                    send_str!(stream, &format!("+OK {} octets\r\n", partial.len()))?;
                    send_str!(stream, &partial)?;
                    send_str!(stream, ".\r\n")?;
                }
            }
            POPCommand::Quit | POPCommand::Disconnect => return Some(recent_id),
            _ => (),
        }
    }
}

fn handle_smtp_connection(mut stream: TcpStream, args: &Args) {
    stream
        .write_all("220 hi welcome to chilis\r\n".as_bytes())
        .expect("Couldn't send welcome message");
    let mut from = "".to_string();
    loop {
        match get_smtp_command(stream.try_clone().unwrap()) {
            //I'm a little confused of why this is necessary, since from is in the RFC email spec
            //so IDK why SMTP gets to be special
            SMTPCommand::Mailfrom(addr) => {
                from = addr;
            }
            SMTPCommand::Data(email_string) => {
                println!("{}", from);
                let (_, account_url) = if args.account.as_deref().is_some() {
                    strip_cred(args.account.as_deref().unwrap())
                } else {
                    strip_cred(&from)
                };
                let auth = string_concat!("Bearer ", args.token.as_deref().unwrap().to_string());
                let msg = Message::parse(email_string.as_bytes()).expect("Error in parsing email");
                let in_reply_to = msg.in_reply_to();
                let references = msg.references();
                let mut status = msg.body_text(0).unwrap().to_string();
                //We set the msg-id to the ID of the mastodon post, and this will
                //be referenced in either the in-reply-to or references header
                let mut reply_id = if in_reply_to != &HeaderValue::Empty {
                    in_reply_to.as_text_list().unwrap()[0]
                } else if references != &HeaderValue::Empty {
                    references.as_text_list().unwrap()[0]
                } else {
                    ""
                };

                //Stolen from https://github.com/crisp-oss/email-reply-parser/lib/regex.js
                //only supports English and certain email clients
                //which I know is not good to use English as a default, but for a project of this scope,I think it's ok.
                //If it's including the original message, disable including original replies in your email client
                let reply_pattern =
                    Regex::new(r"-*\s*(On\s.+\s.+\n?wrote:{0,1})\s{0,1}-*$").unwrap();
                if !reply_id.is_empty() {
                    if let Some(ind) = reply_pattern.find(&status) {
                        status = status.split_at(ind.0).0.to_string()
                    }
                }
                //Strip whitespace and inline image markers from the end of status
                status = status.replace('\u{FFFC}', "");
                status = status.trim_end().to_string();
                //Some clients will add the domain to IDs, so strip that
                if reply_id.contains('@') {
                    reply_id = reply_id.rsplit_once('@').unwrap().0;
                }
                //Make an empty string vector
                let mut media_ids = Vec::new();
                let client = Client::new();
                for attachment in msg.attachments() {
                    if !attachment.is_message() {
                        //Get the attachment info out of the email
                        let bigtype = attachment
                            .content_type()
                            .expect("Error parsing attachment content type")
                            .ctype();
                        let subtype = attachment
                            .content_type()
                            .expect("Error parsing attachment content type")
                            .subtype()
                            .unwrap_or("JPG");
                        let mime = string_concat!(bigtype, "/", subtype);
                        let name = attachment
                            .attachment_name()
                            .unwrap_or("Untitled.jpg")
                            .to_owned();
                        println!("Attachment Name: {:?}", name);
                        println!("Attachment Type: {:?}", mime);
                        //std::fs::write(attachment.attachment_name().unwrap_or("Untitled"), attachment.contents());
                        let content = attachment.contents().to_owned();

                        //Upload the image, we are given an ID in the reply which needs to be included in the post
                        let file_part = Part::bytes(content)
                            .file_name(name)
                            .mime_str(&mime)
                            .unwrap();
                        let form = Form::new().part("file", file_part);
                        let upload_res = client
                            .post(account_url.clone() + "/api/v2/media")
                            .header("Authorization", auth.clone())
                            .multipart(form)
                            .send()
                            .expect("Error uploading image")
                            .text()
                            .unwrap();
                        let ret_vec: Value =
                            serde_json::from_str(&upload_res).expect("Image upload failure");
                        let cur_id = get_str(&ret_vec["id"]).to_owned();
                        media_ids.push(cur_id);
                    }
                }
                //We can only have 4 images per post
                //Hint: if you want to DDOS a mastodon instance, look here :)
                if media_ids.len() > 4 {
                    media_ids = media_ids[0..4].to_vec();
                }
                //Wrap the reply and convert to String
                let in_reply_to_id = if reply_id.is_empty() {
                    None
                } else {
                    Some(reply_id.to_string())
                };
                let form = Status {
                    status,
                    in_reply_to_id,
                    media_ids,
                };
                println!(
                    "{:?}",
                    client
                        .post(account_url + "/api/v1/statuses")
                        .header("Authorization", auth.clone())
                        .json(&form)
                        .send()
                );
            }
            SMTPCommand::Rset => {
                from = "".to_string();
            }
            SMTPCommand::Quit | SMTPCommand::Disconnect => return,
            _ => (),
        }
    }
}

//This is only used in POP3, basically a mini state machine that won't let you do anything before logging in
fn get_login(stream: &mut TcpStream) -> Option<Cred> {
    let mut new_cred = Cred {
        username: String::new(),
        password: String::new(),
    };
    loop {
        match get_pop_command(stream) {
            POPCommand::User(x) => new_cred.username = x,
            POPCommand::Pass(x) => new_cred.password = x,
            POPCommand::Disconnect | POPCommand::Quit => return None,
            _ => (),
        }
        if !new_cred.username.is_empty() && !new_cred.password.is_empty() {
            return Some(new_cred);
        }
    }
}

//The JSON array wasn't given a struct bc I Am Lazy, so this is a helper function to get a string out of a JSON element
fn get_str(element: &Value) -> &str {
    element.as_str().unwrap_or_else(|| {
        println!("Could not parse JSON element: {:?}", element);
        ""
    })
}

fn get_pop_command(stream: &mut TcpStream) -> POPCommand {
    let mut tcp_read = BufReader::new(stream.try_clone().unwrap());
    let mut cur_line = vec![];
    loop {
        cur_line.clear();
        match tcp_read.read_until(b'\n', &mut cur_line) {
            Ok(len) => {
                if len == 0 {
                    println!("Socket closed");
                    return POPCommand::Disconnect;
                }
            }
            Err(err) => {
                println!("TCP error: {:?}", err);
                return POPCommand::Disconnect;
            }
        }
        let cur_line_string = String::from_utf8_lossy(&cur_line);
        let mut split = cur_line_string.split_whitespace();
        match split.next() {
            //USER or PASS shouldn't be received, bc we're already logged in here
            //but we handle it anyway because we're nice
            Some("USER") => {
                send_str!(stream, "+OK send PASS\r\n");
                return POPCommand::User(split.next().unwrap_or("").to_string());
            }
            Some("PASS") => return POPCommand::Pass(split.next().unwrap_or("").to_string()),
            //in terms of capabilities we have no capabilities
            Some("CAPA") => {
                send_str!(
                    stream,
                    "+OK Capability list follows\r\nUSER\r\nTOP\r\nUIDL\r\n.\r\n"
                );
                return POPCommand::Capa;
            }
            //TODO: should we send something back here?
            Some("QUIT") => return POPCommand::Quit,
            Some("STAT") => return POPCommand::Stat,
            Some("NOOP") => {
                send_str!(stream, "+OK\r\n");
                return POPCommand::Noop;
            }
            Some("RSET") => {
                send_str!(stream, "+OK\r\n");
                return POPCommand::Rset;
            }
            Some("APOP") => {
                send_str!(stream, "-ERR Server does not support APOP\r\n");
                return POPCommand::Apop;
            }
            //LIST (gives info about a message) and RETR (gives the message) can be sent with an index or not
            //so we either parse the index or return 0 (bc it's one indexed)
            Some("LIST") => {
                return POPCommand::List(split.next().unwrap_or("0").parse::<u32>().unwrap_or(0))
            }
            Some("RETR") => {
                return POPCommand::Retr(split.next().unwrap_or("0").parse::<u32>().unwrap_or(0))
            }
            Some("DELE") => {
                send_str!(stream, "+OK\r\n");
                return POPCommand::Dele(split.next().unwrap_or("0").parse::<u32>().unwrap_or(0));
            }
            Some("UIDL") => {
                return POPCommand::Uidl(split.next().unwrap_or("0").parse::<u32>().unwrap_or(0))
            }
            Some("TOP") => {
                //send_str!(stream, "-ERR Server does not support TOP\r\n");
                let msg = split.next().unwrap_or("0").parse::<u32>().unwrap_or(0);
                let n = split.next().unwrap_or("0").parse::<u32>().unwrap_or(0);
                return POPCommand::Top { msg, n };
            }
            Some("AUTH") => {
                send_str!(stream, "-ERR Server does not support AUTH\r\n");
                return POPCommand::Auth;
            }
            _ => (),
        }
    }
}

fn get_smtp_command(mut stream: TcpStream) -> SMTPCommand {
    let re = Regex::new(r"(?<=\<)(.*?)(?=\>)").unwrap();
    let mut tcp_read = BufReader::new(stream.try_clone().unwrap());
    let mut cur_line_bytes = vec![];
    loop {
        cur_line_bytes.clear();
        match tcp_read.read_until(b'\n', &mut cur_line_bytes) {
            Ok(len) => {
                if len == 0 {
                    println!("Socket closed");
                    return SMTPCommand::Disconnect;
                }
            }
            Err(err) => {
                println!("TCP error: {:?}", err);
                return SMTPCommand::Disconnect;
            }
        }
        let cur_line = String::from_utf8_lossy(&cur_line_bytes);
        if cur_line.starts_with("MAIL FROM:") {
            send_str!(stream, "250 OK\r\n");
            return SMTPCommand::Mailfrom(
                re.captures(&cur_line).unwrap().at(0).unwrap().to_string(),
            );
        } else if cur_line.starts_with("RCPT TO:") {
            send_str!(stream, "250 OK\r\n");
            return SMTPCommand::RcptTo(re.captures(&cur_line).unwrap().at(0).unwrap().to_string());
        } else if cur_line.starts_with("DATA") {
            send_str!(stream, "354 Send message content\r\n");
            let mut ret = "".to_string();
            let mut data_cur_line_bytes = vec![];
            //This isn't very clean, and basically is another state machine within this one
            //but I don't /particularly/ care rn
            loop {
                match tcp_read.read_until(b'\n', &mut data_cur_line_bytes) {
                    Ok(len) => {
                        if len == 0 {
                            println!("Socket closed");
                            return SMTPCommand::Disconnect;
                        }
                    }
                    Err(err) => {
                        println!("TCP error: {:?}", err);
                        return SMTPCommand::Disconnect;
                    }
                }
                let cur_line = String::from_utf8_lossy(&data_cur_line_bytes);
                //single dot represents the end of a message
                if cur_line == ".\r\n" {
                    break;
                } else {
                    ret = string_concat!(ret, cur_line);
                }
                data_cur_line_bytes.clear();
            }
            send_str!(stream, "250 OK\r\n");
            return SMTPCommand::Data(ret);
        } else if cur_line.starts_with("HELO") {
            send_str!(stream, "250 mop3 whats poppin\r\n");
            return SMTPCommand::Helo;
        } else if cur_line.starts_with("EHLO") {
            send_str!(
                stream,
                "250-mop3 whats poppin\r\n250-SIZE 5000000\r\n250 OK\r\n"
            );
            return SMTPCommand::Ehlo;
        } else if cur_line.starts_with("NOOP") {
            send_str!(stream, "250 OK\r\n");
            return SMTPCommand::Noop;
        } else if cur_line.starts_with("QUIT") {
            send_str!(stream, "221 good bye\r\n");
            return SMTPCommand::Quit;
        } else if cur_line.starts_with("RSET") {
            send_str!(stream, "250 OK\r\n");
            return SMTPCommand::Rset;
        }
    }
}

//returns account domain and instance url
fn strip_cred(username: &str) -> (String, String) {
    //We only want the server domain, strip the account name
    let username = username
        .rsplit_once('@')
        .map(|parts| parts.1)
        .unwrap_or(username)
        .to_owned();
    //and add the protocol
    let username_domain = if !username.contains("https://") {
        format!("https://{}", username)
    } else {
        username.clone()
    };
    (username, username_domain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_cred() {
        assert_eq!(
            strip_cred("user@example.com"),
            ("example.com".to_string(), "https://example.com".to_string())
        )
    }
}
