use std::{
    net::{TcpListener,TcpStream},
    io::{prelude::*, BufReader}
};

use reqwest::blocking::Client;
use serde_json::Value;
use string_concat::*;
use html2text::from_read;
use chrono::DateTime;
use chrono::naive::NaiveDateTime;
use chrono::offset::Utc;
use clap::Parser;
use deunicode::deunicode;

#[derive(Debug)]
struct Cred{
    username: String,
    password: String
}

#[allow(unused)]
enum POPCommand{
    Quit, Stat, List(u32), Retr(u32), Dele(u32), Noop, Rset, Top{msg: u32, n: u32}, 
    Uidl(u32), User(String), Pass(String), Apop, Disconnect, Capa
}

//This basically converts the Err to Option, without having to borrow stream
//A little hacky?
macro_rules! send_str{
    ($stream:expr,$msg:expr) => {
        if $stream.write_all($msg.as_bytes()).is_ok(){Some(())} else {None}
    };
}

#[derive(Parser, Debug)]
#[command(name = "MOP3")]
#[command(author = "Nathan Kiesman. <nkizz@tacobelllabs.net>")]
#[command(version = "0.1")]
#[command(about = "Mastodon to POP3 gateway", long_about = None)]
#[command(author, version, about, long_about = None)]
struct Args {
   /// If set, mastodon account to connect to, ex. user@example.com. Otherwise, uses username from POP3 connection
   #[arg(long)]
   account: Option<String>,
   /// If set, mastodon token to connect with. Otherwise, uses password from POP3 connection
   #[arg(long)]
   token: Option<String>,
   /// Address to listen on, defaults to localhost
   #[arg(long)]
   address: Option<String>,
   /// Only send ASCII to clients, gracefully converts unicode
   #[arg(short, long)]
    ascii: bool
}

fn main() {
    let args = Args::parse();
    //Most recent ID fetched, passed to API call to reduce server load
    let mut recent = "".to_string();
    let account = args.address.as_deref().unwrap_or("127.0.0.1:110");
    println!("Listening on {}", account);
    loop{
        let listener = TcpListener::bind(account).unwrap();
        for stream in listener.incoming() {
            let stream = stream.unwrap();
            if let Some(new_recent) = handle_connection(&args, stream, recent.clone()) {
                recent = new_recent;
            };
        }
    }
}

fn handle_connection(args: &Args, mut stream: TcpStream, mut recent_id: String) -> Option<String> {
    stream.write_all("+OK MOP3 ready\r\n".as_bytes()).expect("Couldn't send welcome message");

    let new_cred_res = get_login(stream.try_clone().expect("Couldn't clone stream :("));
    //Make sure we didn't drop the connection
    let mut new_cred = match new_cred_res {
        Some(cred) => cred,
        None => return None
    };
    //If credentials have been passed in on the CLI, use them
    if args.account.as_deref().is_some(){
        new_cred.username = args.account.as_deref()?.to_string();
    }
    if args.token.as_deref().is_some(){
        new_cred.password = args.token.as_deref()?.to_string();
    }
    //We only want the server domain, strip the account name
    if new_cred.username.contains('@'){
        new_cred.username = new_cred.username.rsplit_once('@')?.1.to_owned();
    }
    //and add the protocol
    if !new_cred.username.contains("https://"){
        new_cred.username = String::from("https://") + &new_cred.username;
    }
    
    let client = Client::new();
    
    //Verify account and get user's display name
    let account_str = client
        .get(new_cred.username.to_owned() + "/api/v1/accounts/verify_credentials")
        .header("Authorization", "Bearer ".to_owned() + &new_cred.password)
        .send().expect("Could not verify credentials").text().unwrap();
    let account: Value = serde_json::from_str(&account_str).expect("Server sent malformed JSON");
    
    let account_disp_name = get_str(&account["display_name"]);
    let account_domain = &new_cred.username.to_owned()[8..];
    let account_addr = string_concat!(get_str(&account["username"]), "@", account_domain);
    
    //Get timeline
    let since_id = if recent_id.is_empty() {
        "".to_string()
    } else {string_concat!("&since_id=".to_owned() , recent_id)};
    let timeline_str = client
        .get(new_cred.username + "/api/v1/timelines/home?limit=40" + &since_id)
        .header("Authorization", "Bearer ".to_owned() + &new_cred.password)
        .send().expect("Could not retreive timeline").text().unwrap();
    println!("{}", timeline_str);
    let timeline: Vec<Value> = serde_json::from_str(&timeline_str).expect("Server sent malformed JSON");
    
    //Total size of all emails, needs to be reported back
    let mut post_size=0;
    let mut emails: Vec<String> = vec![];
    for post in &timeline{
        println!("{}", get_str(&post["created_at"]));
        let reply_to = if post["in_reply_to_id"] == Value::Null {"".to_string()} else {
            string_concat!("In-Reply-To: <", get_str(&post["in_reply_to_id"]), "@", account_domain, ">\r\n")};
        //If this is a reblog, get text & images from the reblog
        let (content,media_vec, subject) = if get_str(&post["content"]) == ""{
            (from_read(get_str(&post["reblog"]["content"]).as_bytes(),78).replace("\n", "\r\n"),
             &post["reblog"]["media_attachments"],
             "Boost")
        } else {
            (from_read(get_str(&post["content"]).as_bytes(),78).replace("\n", "\r\n"),
             &post["media_attachments"],
            "Post")
        };
        //Get URLs of any media
        let mut media_urls = "".to_string();
        for media in media_vec.as_array().expect("Server sent malformed JSON (no media array)"){
            media_urls = string_concat!(media_urls, "\r\n", get_str(&media["url"]));
        }
        //oh lawd he comin
        let mut message = string_concat!(
        "From: ", get_str(&post["account"]["display_name"]), " <", get_str(&post["account"]["acct"]), 
        ">\r\nTo: ", account_disp_name, " <", account_addr,
        ">\r\nSubject: ", subject,
        //Convert date from ISO to RFC2822 as required by email
        "\r\nDate: ", DateTime::<Utc>::from_utc(NaiveDateTime::parse_from_str(get_str(&post["created_at"]), "%Y-%m-%dT%H:%M:%S%.3fZ").expect("Server sent unexpected time format"), Utc).to_rfc2822(),
        "\r\nMessage-ID: <", get_str(&post["id"]), "@", account_domain, ">\r\n",
        reply_to, "\r\n", 
        content, 
        media_urls, "\r\n");
        if args.ascii {message = deunicode(&message);}
        post_size += message.len();
        emails.push(message);
    }
    send_str!(stream,"+OK MOP3 READY, MESSAGES FETCHED\r\n")?;
    //process commands as we get them
    loop{
        //what if we kissed in The TRANSACTION State
        match get_pop_command(stream.try_clone().unwrap()){
            POPCommand::List(index) => {
                let i = index as usize;
                if i != 0 {
                    if i > emails.len(){
                        send_str!(stream, "-ERR no such message\r\n")?;
                    } else {
                        send_str!(stream, &format!("+OK {} {}\r\n", i, emails[i-1].len()))?;
                    }
                } else {
                    send_str!(stream, &format!("+OK {} messages ({} octets)\r\n", emails.len(), post_size))?;
                    for (i, msg) in emails.iter().enumerate(){
                        send_str!(stream, &format!("{} {}\r\n",i+1,msg.len()))?;
                    }
                    send_str!(stream, ".\r\n")?;
                }}

            POPCommand::Retr(i) => {
                let ind = (i-1) as usize;
                send_str!(stream, &format!("+OK {} octets\r\n", emails[ind].len()))?;
                send_str!(stream, &emails[ind])?;
                send_str!(stream, ".\r\n")?;
                recent_id = get_str(&timeline[0]["id"]).to_string();
            },

            POPCommand::Stat => send_str!(stream, &format!("+OK {} {}\r\n", emails.len(), post_size))?,
            POPCommand::Uidl(ind) => {
                let i = ind as usize;
                if i != 0 {
                    if i > emails.len(){
                        send_str!(stream, "-ERR no such message\r\n")?;
                    } else {
                        send_str!(stream, &string_concat!("+OK ", ind.to_string(), " ", get_str(&timeline[i-1]["id"]), "@", account_domain, "\r\n"))?;
                    }
                } else {
                    send_str!(stream, "+OK\r\n")?;
                    for (i, msg) in timeline.iter().enumerate(){
                        send_str!(stream, &format!("{} {}@{}\r\n",i+1,get_str(&msg["id"]),account_domain))?;
                    }
                    send_str!(stream, ".\r\n")?;
                }
            },
            POPCommand::Quit | POPCommand::Disconnect => return Some(recent_id),
            _ => (),
        }
    }
}

fn get_login(stream: TcpStream) -> Option<Cred> {
    let mut new_cred = Cred{username: String::new(), password: String::new()};
    loop{
        match get_pop_command(stream.try_clone().unwrap()){
            POPCommand::User(x) => new_cred.username = x,
            POPCommand::Pass(x) => new_cred.password = x,
            POPCommand::Disconnect | POPCommand::Quit => return None,
            _ => (),
        }
        if !new_cred.username.is_empty() && !new_cred.password.is_empty(){
            return Some(new_cred)
        }
    }
}

fn get_str(element: &Value) -> &str {
    let str_option = element.as_str();
    if str_option.is_some() {return str_option.unwrap().clone();}
    else {println!("Could not parse JSON element: {:?}", element); return "";}
}

fn get_pop_command(mut stream: TcpStream) -> POPCommand {
    let mut tcp_read = BufReader::new(stream.try_clone().unwrap());
    let mut cur_line = vec![];
    loop {
        cur_line.clear();
        match tcp_read.read_until(b'\n', &mut cur_line){
            Ok(len) => if len == 0{println!("Socket closed"); return POPCommand::Disconnect;},
            Err(err) => {println!("TCP error: {:?}", err); return POPCommand::Disconnect;}
        }
        let cur_line_string = String::from_utf8_lossy(&cur_line);
        let mut split = cur_line_string.split_whitespace();
        match split.next() {
            Some("USER") => {send_str!(stream, "+OK send PASS\r\n");
                            return POPCommand::User(split.next().unwrap_or("").to_string())},
            Some("PASS") => return POPCommand::Pass(split.next().unwrap_or("").to_string()),
            Some("CAPA") => {
                send_str!(stream, "+OK Capability list follows\r\nUSER\r\n.\r\n");
                return POPCommand::Capa},
            Some("QUIT") => return POPCommand::Quit,
            Some("STAT") => return POPCommand::Stat,
            Some("NOOP") => {send_str!(stream, "+OK\r\n");return POPCommand::Noop},
            Some("RSET") => {send_str!(stream, "+OK\r\n");return POPCommand::Rset},
            Some("APOP") => {send_str!(stream, "-ERR Server does not support APOP\r\n");return POPCommand::Apop},
            Some("LIST") => return POPCommand::List(split.next().unwrap_or("0").parse::<u32>().unwrap_or(0)),
            Some("RETR") => return POPCommand::Retr(split.next().unwrap_or("0").parse::<u32>().unwrap_or(0)),
            Some("DELE") => {send_str!(stream, "+OK\r\n"); return POPCommand::Dele(split.next().unwrap_or("0").parse::<u32>().unwrap_or(0))},
            Some("UIDL") => return POPCommand::Uidl(split.next().unwrap_or("0").parse::<u32>().unwrap_or(0)),
            Some("TOP") => {
                send_str!(stream, "-ERR Server does not support TOP\r\n");
                let msg = split.next().unwrap_or("0").parse::<u32>().unwrap_or(0);
                let n   = split.next().unwrap_or("0").parse::<u32>().unwrap_or(0);
                return POPCommand::Top {msg, n}
            },
            _ => (),
        }
    }
}