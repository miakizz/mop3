# MOP3
A Mastodon to POP3 Gateway

MOP3 is a toy-ish, standard complient-ish server which speaks POP3, and serves data from your home Mastodon server. This enables email clients made as early as the 1980s to receive and display Mastodon posts. MOP3 is intended less for everyday use, and more for retro-computing, as it can be difficult to setup an email server to play with old clients, and modern email servers want silly things like security. With that said, it could be used as a true Mastodon client, as even modern email clients like Apple's Mail.app support POP3. Emails are served in plain-text with the HTML stripped, and optionally, with all Unicode converted to 7 bit ASCII. Currently it does support threading and image URLs; future additions could include TLS/SSL support, inline HTML, and even posting via SMTP.
![Outlook Express displaying Mastodon posts](/screenshots/mop3-win.png?raw=true "MOP3 on Outlook Express")
![Mail.app displaying Mastodon posts](/screenshots/mop3-mac.png?raw=true "MOP3 on MacOS Monterey Mail")

## Installation
This is written in Rust, so running `cargo install mop3` on your host should install it. If not, downloading the repo and running `cargo build` should also work. Binaries can be provided if desired, just drop me a line.

## Usage
This requires an access token, which can be obtained in Preferences -> Development -> New Application on your Mastodon instance. MOP3 only requires read permissions, so I reccomend not giving it any of the other ones. The client key and secret are not required.
`mop3 --help` will give you all of the important runtime flags, none of which are required, but `--token` can be especially useful if you don't want to type in your access token on your retro machine. By default the server binds to localhost, post 110. To enable connections from other clients, pass the option `--address 0.0.0.0:110`.
To connect to it, point your client at the server ip/port, with the username of "username@mastoinstance.com" and the password of your account token, no SSL/TLS/SPA. Some clients will not include the domain name in the username by default, so make sure it includes both parts, or use `--user`.
On the first connection, MOP3 will fetch the last 40 posts on your timeline, and all posts since the last time it connected on every subsequent connection. It can't differentiate between clients, so the server will need to be restarted to refetch posts on a new client.

## Disclaimer
You run this application _at your own risk_. MOP3 is my first Rust application, and so probably contains code slightly below world class levels. It is also speaking a protocol from the 90s/70s, with no security, and little authentication. I don't reccomend running this on the internet. I also tried to be friendly with my use of the Mastodon API, but I'm not responsible for any DMs from your sysop if it does something weird. However, the code is relatively simple, it's been tested, and especially with the `--token` option, not passing around secret data, so it _should_ be perfectly safe to run on a LAN.

