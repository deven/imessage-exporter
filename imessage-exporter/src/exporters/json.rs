#![allow(unused_imports)]

use std::{
    borrow::Cow,
    collections::{
        hash_map::Entry::{Occupied, Vacant},
        HashMap,
    },
    fs::File,
    io::{BufWriter, Write},
};

use serde_json::json;

use crate::{
    app::{
        error::RuntimeError, progress::build_progress_bar_export, runtime::Config,
        sanitizers::sanitize_json,
    },
    exporters::exporter::{BalloonFormatter, Exporter, TextEffectFormatter, Writer},
};

use imessage_database::{
    error::{plist::PlistParseError, table::TableError},
    message_types::{
        app::AppMessage,
        app_store::AppStoreMessage,
        collaboration::CollaborationMessage,
        digital_touch::{self, DigitalTouch},
        edited::{EditStatus, EditedMessage},
        expressives::{BubbleEffect, Expressive, ScreenEffect},
        handwriting::HandwrittenMessage,
        music::MusicMessage,
        placemark::PlacemarkMessage,
        text_effects::{Animation, Style, TextEffect, Unit},
        url::URLMessage,
        variants::{Announcement, BalloonProvider, CustomBalloon, URLOverride, Variant},
    },
    tables::{
        attachment::{Attachment, MediaType},
        messages::{models::BubbleComponent, Message},
        table::{Table, FITNESS_RECEIVER, ME, ORPHANED, YOU},
    },
    util::{
        dates::{format, get_local_time, readable_diff, TIMESTAMP_FACTOR},
        plist::parse_plist,
    },
};

const HEADER: &str = "[\n  ";
const SEPARATOR: &str = ",\n  ";
const FOOTER: &str = "\n]\n";

pub struct JSON<'a> {
    /// Data that is setup from the application's runtime
    pub config: &'a Config,
    /// Handles to files we want to write messages to
    /// Map of resolved chatroom file location to a buffered writer
    pub files: HashMap<String, BufWriter<File>>,
}

impl<'a> Exporter<'a> for JSON<'a> {
    /// Create a new exporter with references to the cached data
    fn new(config: &'a Config) -> Result<Self, RuntimeError> {
        Ok(JSON {
            config,
            files: HashMap::new(),
        })
    }

    /// Begin iterating over the messages table
    fn iter_messages(&mut self) -> Result<(), RuntimeError> {
        // Tell the user what we are doing
        eprintln!(
            "Exporting to {} as JSON...",
            self.config.options.export_path.display()
        );

        // Keep track of current message ROWID
        let mut current_message_row = -1;

        // Set up progress bar
        let mut current_message = 0;
        let total_messages =
            Message::get_count(&self.config.db, &self.config.options.query_context)
                .map_err(RuntimeError::DatabaseError)?;
        let pb = build_progress_bar_export(total_messages);

        let mut statement =
            Message::stream_rows(&self.config.db, &self.config.options.query_context)
                .map_err(RuntimeError::DatabaseError)?;

        let messages = statement
            .query_map([], |row| Ok(Message::from_row(row)))
            .map_err(|err| RuntimeError::DatabaseError(TableError::Messages(err)))?;

        for message in messages {
            let mut msg = Message::extract(message).map_err(RuntimeError::DatabaseError)?;

            // Early escape if we try and render the same message GUID twice
            // See https://github.com/ReagentX/imessage-exporter/issues/135 for rationale
            if msg.rowid == current_message_row {
                current_message += 1;
                continue;
            }
            current_message_row = msg.rowid;

            // Generate the text of the message
            let _ = msg.generate_text(&self.config.db);

            // Render the announcement in-line
            if msg.is_announcement() {
                let announcement = self.format_announcement(&msg);
                JSON::write_to_file(self.get_or_create_file(&msg)?, &announcement)?;
            }
            // Message replies and tapbacks are rendered in context, so no need to render them separately
            else if !msg.is_tapback() {
                let message = self
                    .format_message(&msg, 0)
                    .map_err(RuntimeError::DatabaseError)?;
                JSON::write_to_file(self.get_or_create_file(&msg)?, &message)?;
            }
            current_message += 1;
            if current_message % 99 == 0 {
                pb.set_position(current_message);
            }
        }
        pb.finish();

        eprintln!("Writing JSON footers...");
        for (_, buf) in self.files.iter_mut() {
            JSON::write_to_file(buf, FOOTER)?;
        }

        Ok(())
    }

    /// Create a file for the given chat, caching it so we don't need to build it later
    fn get_or_create_file(
        &mut self,
        message: &Message,
    ) -> Result<&mut BufWriter<File>, RuntimeError> {
        let filename = match self.config.conversation(message) {
            Some((chatroom, _)) => self.config.filename(chatroom),
            None => ORPHANED.to_string(),
        };
        match self.files.entry(filename.clone()) {
            Occupied(entry) => {
                let buf = entry.into_mut();
                JSON::write_to_file(buf, SEPARATOR)?;
                Ok(buf)
            }
            Vacant(entry) => {
                let mut path = self.config.options.export_path.clone();
                path.push(filename);
                path.set_extension("json");

                // If the file already exists, don't write the headers again
                // This can happen if multiple chats use the same group name
                let file_exists = path.exists();

                let file = File::options()
                    .append(true)
                    .create(true)
                    .open(&path)
                    .map_err(|err| RuntimeError::CreateError(err, path))?;

                let mut buf = BufWriter::new(file);

                // Write header or separator.
                if file_exists {
                    JSON::write_to_file(&mut buf, SEPARATOR)?;
                } else {
                    JSON::write_to_file(&mut buf, HEADER)?;
                }

                Ok(entry.insert(buf))
            }
        }
    }
}

impl<'a> Writer<'a> for JSON<'a> {
    fn format_message(&self, message: &Message, indent_size: usize) -> Result<String, TableError> {
        //let indent = String::from_iter((0..indent_size).map(|_| " "));
        let mut formatted_message = serde_json::to_string(message)?;
        Ok(formatted_message)
    }

    /// Format a single attachment into a JSON object
    fn format_attachment(
        &self,
        attachment: &'a mut Attachment,
        message: &'a Message,
    ) -> Result<String, &'a str> {
        // Copy the file, if requested
        self.config
            .options
            .attachment_manager
            .handle_attachment(message, attachment, self.config)
            .ok_or(attachment.filename())?;

        match serde_json::to_string(attachment) {
            Ok(formatted_attachment) => Ok(formatted_attachment),
            Err(_) => Err("Failed to serialize message to JSON"),
        }
    }

    /// Format a sticker, possibly by reading the disk
    fn format_sticker(&self, sticker: &'a mut Attachment, message: &Message) -> String {
        match self.format_attachment(sticker, message) {
            Ok(sticker_embed) => sticker_embed,
            Err(embed) => embed.to_string(),
        }
    }

    /// Format an app message by parsing some of its fields
    fn format_app(
        &self,
        message: &'a Message,
        attachments: &mut Vec<Attachment>,
        indent: &str,
    ) -> Result<String, PlistParseError> {
        if let Variant::App(balloon) = message.variant() {
            let mut app_bubble = String::new();

            // Handwritten messages use a different payload type, so check that first
            if message.is_handwriting() {
                if let Some(payload) = message.raw_payload_data(&self.config.db) {
                    return match HandwrittenMessage::from_payload(&payload) {
                        Ok(bubble) => Ok(self.format_handwriting(message, &bubble, indent)),
                        Err(why) => Err(PlistParseError::HandwritingError(why)),
                    };
                }
            }

            if message.is_digital_touch() {
                if let Some(payload) = message.raw_payload_data(&self.config.db) {
                    return match digital_touch::from_payload(&payload) {
                        Some(bubble) => Ok(self.format_digital_touch(message, &bubble, indent)),
                        None => Err(PlistParseError::DigitalTouchError),
                    };
                }
            }

            if let Some(payload) = message.payload_data(&self.config.db) {
                // Handle URL messages separately since they are a special case
                let res = if message.is_url() {
                    let parsed = parse_plist(&payload)?;
                    let bubble = URLMessage::get_url_message_override(&parsed)?;
                    match bubble {
                        URLOverride::Normal(balloon) => self.format_url(message, &balloon, indent),
                        URLOverride::AppleMusic(balloon) => self.format_music(&balloon, indent),
                        URLOverride::Collaboration(balloon) => {
                            self.format_collaboration(&balloon, indent)
                        }
                        URLOverride::AppStore(balloon) => self.format_app_store(&balloon, indent),
                        URLOverride::SharedPlacemark(balloon) => {
                            self.format_placemark(&balloon, indent)
                        }
                    }
                // Handwriting uses a different payload type than the rest of the branches
                } else {
                    // Handle the app case
                    let parsed = parse_plist(&payload)?;
                    match AppMessage::from_map(&parsed) {
                        Ok(bubble) => match balloon {
                            CustomBalloon::Application(bundle_id) => {
                                self.format_generic_app(&bubble, bundle_id, attachments, indent)
                            }
                            CustomBalloon::ApplePay => self.format_apple_pay(&bubble, indent),
                            CustomBalloon::Fitness => self.format_fitness(&bubble, indent),
                            CustomBalloon::Slideshow => self.format_slideshow(&bubble, indent),
                            CustomBalloon::CheckIn => self.format_check_in(&bubble, indent),
                            CustomBalloon::FindMy => self.format_find_my(&bubble, indent),
                            CustomBalloon::Handwriting => unreachable!(),
                            CustomBalloon::DigitalTouch => unreachable!(),
                            CustomBalloon::URL => unreachable!(),
                        },
                        Err(why) => return Err(why),
                    }
                };
                app_bubble.push_str(&res);
            } else {
                // Sometimes, URL messages are missing their payloads
                if message.is_url() {
                    if let Some(text) = &message.text {
                        return Ok(text.to_string());
                    }
                }
                return Err(PlistParseError::NoPayload);
            }
            Ok(app_bubble)
        } else {
            Err(PlistParseError::WrongMessageType)
        }
    }

    /// Format a tapback (displayed under a message)
    fn format_tapback(&self, message: &Message) -> Result<String, TableError> {
        let formatted_message = serde_json::to_string(message)?;
        Ok(formatted_message)
    }

    /// Format an expressive message
    fn format_expressive(&self, message: &'a Message) -> &'a str {
        match message.get_expressive() {
            Expressive::Screen(effect) => match effect {
                ScreenEffect::Confetti => "Sent with Confetti",
                ScreenEffect::Echo => "Sent with Echo",
                ScreenEffect::Fireworks => "Sent with Fireworks",
                ScreenEffect::Balloons => "Sent with Balloons",
                ScreenEffect::Heart => "Sent with Heart",
                ScreenEffect::Lasers => "Sent with Lasers",
                ScreenEffect::ShootingStar => "Sent with Shooting Star",
                ScreenEffect::Sparkles => "Sent with Sparkles",
                ScreenEffect::Spotlight => "Sent with Spotlight",
            },
            Expressive::Bubble(effect) => match effect {
                BubbleEffect::Slam => "Sent with Slam",
                BubbleEffect::Loud => "Sent with Loud",
                BubbleEffect::Gentle => "Sent with Gentle",
                BubbleEffect::InvisibleInk => "Sent with Invisible Ink",
            },
            Expressive::Unknown(effect) => effect,
            Expressive::None => "",
        }
    }

    /// Format an announcement message
    fn format_announcement(&self, message: &'a Message) -> String {
        match serde_json::to_string(message) {
            Ok(formatted_message) => formatted_message,
            Err(_) => "Failed to serialize message to JSON".to_string(),
        }
    }

    /// Format a `SharePlay` message
    fn format_shareplay(&self) -> &str {
        "SharePlay Message Ended"
    }

    /// Format a legacy Shared Location message
    fn format_shared_location(&self, message: &'a Message) -> &str {
        // Handle Shared Location
        if message.started_sharing_location() {
            return "Started sharing location!";
        } else if message.stopped_sharing_location() {
            return "Stopped sharing location!";
        }
        "Shared location!"
    }

    /// Format an edited message
    fn format_edited(
        &self,
        message: &'a Message,
        _edited_message: &'a EditedMessage,
        _message_part_idx: usize,
        _indent: &str,
    ) -> Option<String> {
        let formatted_message = serde_json::to_string(message).ok()?;
        Some(formatted_message)
    }

    /// Format some attributed text
    fn format_attributed(&'a self, text: &'a str, attribute: &'a TextEffect) -> Cow<'a, str> {
        let formatted_text = serde_json::to_string(text)
            .unwrap_or_else(|_| "\"Error serializing text\"".to_string());
        let formatted_attribute = serde_json::to_string(attribute)
            .unwrap_or_else(|_| "\"Error serializing text_effect\"".to_string());
        let formatted_message = format!(
            "{{ text: {}, text_effect: {} }}",
            formatted_text, formatted_attribute
        );
        Cow::Owned(formatted_message)
    }

    fn write_to_file(file: &mut BufWriter<File>, text: &str) -> Result<(), RuntimeError> {
        file.write_all(text.as_bytes())
            .map_err(RuntimeError::DiskError)
    }
}

impl<'a> BalloonFormatter<&'a str> for JSON<'a> {
    /// Format a URL message
    fn format_url(&self, _message: &Message, balloon: &URLMessage, _indent: &str) -> String {
        let formatted_balloon = serde_json::to_string(balloon)
            .unwrap_or_else(|_| "\"Error serializing balloon\"".to_string());
        formatted_balloon
    }

    /// Format an Apple Music message
    fn format_music(&self, balloon: &MusicMessage, _indent: &str) -> String {
        let formatted_balloon = serde_json::to_string(balloon)
            .unwrap_or_else(|_| "\"Error serializing balloon\"".to_string());
        formatted_balloon
    }

    /// Format a Rich Collaboration message
    fn format_collaboration(&self, balloon: &CollaborationMessage, _indent: &str) -> String {
        let formatted_balloon = serde_json::to_string(balloon)
            .unwrap_or_else(|_| "\"Error serializing balloon\"".to_string());
        formatted_balloon
    }

    /// Format an App Store link
    fn format_app_store(&self, balloon: &AppStoreMessage, _indent: &str) -> String {
        let formatted_balloon = serde_json::to_string(balloon)
            .unwrap_or_else(|_| "\"Error serializing balloon\"".to_string());
        formatted_balloon
    }

    /// Format a shared location message
    fn format_placemark(&self, balloon: &PlacemarkMessage, _indent: &str) -> String {
        let formatted_balloon = serde_json::to_string(balloon)
            .unwrap_or_else(|_| "\"Error serializing balloon\"".to_string());
        formatted_balloon
    }

    /// Format a handwritten note message
    fn format_handwriting(
        &self,
        _message: &Message,
        balloon: &HandwrittenMessage,
        _indent: &str,
    ) -> String {
        let formatted_balloon = serde_json::to_string(balloon)
            .unwrap_or_else(|_| "\"Error serializing balloon\"".to_string());
        formatted_balloon
    }

    /// Format a digital touch message
    fn format_digital_touch(
        &self,
        _message: &Message,
        balloon: &DigitalTouch,
        _indent: &str,
    ) -> String {
        let formatted_balloon = serde_json::to_string(balloon)
            .unwrap_or_else(|_| "\"Error serializing balloon\"".to_string());
        formatted_balloon
    }

    /// Format an Apple Pay message
    fn format_apple_pay(&self, balloon: &AppMessage, _indent: &str) -> String {
        let formatted_balloon = serde_json::to_string(balloon)
            .unwrap_or_else(|_| "\"Error serializing balloon\"".to_string());
        formatted_balloon
    }

    /// Format a Fitness message
    fn format_fitness(&self, balloon: &AppMessage, _indent: &str) -> String {
        let formatted_balloon = serde_json::to_string(balloon)
            .unwrap_or_else(|_| "\"Error serializing balloon\"".to_string());
        formatted_balloon
    }

    /// Format a Photo Slideshow message
    fn format_slideshow(&self, balloon: &AppMessage, _indent: &str) -> String {
        let formatted_balloon = serde_json::to_string(balloon)
            .unwrap_or_else(|_| "\"Error serializing balloon\"".to_string());
        formatted_balloon
    }

    /// Format a Find My message
    fn format_find_my(&self, balloon: &AppMessage, _indent: &str) -> String {
        let formatted_balloon = serde_json::to_string(balloon)
            .unwrap_or_else(|_| "\"Error serializing balloon\"".to_string());
        formatted_balloon
    }

    /// Format a Check In message
    fn format_check_in(&self, balloon: &AppMessage, _indent: &str) -> String {
        let formatted_balloon = serde_json::to_string(balloon)
            .unwrap_or_else(|_| "\"Error serializing balloon\"".to_string());
        formatted_balloon
    }

    /// Format a generic app, generally third party
    fn format_generic_app(
        &self,
        balloon: &AppMessage,
        _bundle_id: &str,
        _attachments: &mut Vec<Attachment>,
        _indent: &str,
    ) -> String {
        let formatted_balloon = serde_json::to_string(balloon)
            .unwrap_or_else(|_| "\"Error serializing balloon\"".to_string());
        formatted_balloon
    }
}

impl<'a> TextEffectFormatter for JSON<'a> {
    /// Format message text containing a [`Mention`](imessage_database::message_types::text_effects::TextEffect::Mention)
    fn format_mention(&self, text: &str, mentioned: &str) -> String {
        format!("<span title=\"{mentioned}\"><b>{text}</b></span>")
    }

    /// Format message text containing a [`Link`](imessage_database::message_types::text_effects::TextEffect::Link)
    fn format_link(&self, text: &str, url: &str) -> String {
        format!("<a href=\"{url}\">{text}</a>")
    }

    /// Format message text containing an [`OTP`](imessage_database::message_types::text_effects::TextEffect::OTP)
    fn format_otp(&self, text: &str) -> String {
        format!("<u>{text}</u>")
    }

    /// Format message text containing a [`Conversion`](imessage_database::message_types::text_effects::TextEffect::Conversion)
    fn format_conversion(&self, text: &str, unit: &Unit) -> String {
        format!("<u>{text}</u>")
    }

    /// Format message text containing some [`Styles`](imessage_database::message_types::text_effects::TextEffect::Styles)
    fn format_styles(&self, text: &str, styles: &[Style]) -> String {
        let (prefix, suffix): (String, String) = styles.iter().rev().fold(
            (String::new(), String::new()),
            |(mut prefix, mut suffix), style| {
                let (open, close) = match style {
                    Style::Bold => ("<b>", "</b>"),
                    Style::Italic => ("<i>", "</i>"),
                    Style::Strikethrough => ("<s>", "</s>"),
                    Style::Underline => ("<u>", "</u>"),
                };
                prefix.push_str(open);
                suffix.insert_str(0, close);
                (prefix, suffix)
            },
        );

        format!("{prefix}{text}{suffix}")
    }

    /// Format [`Animated`](imessage_database::message_types::text_effects::TextEffect::Animated) message text
    fn format_animated(&self, text: &str, animation: &Animation) -> String {
        format!("<span class=\"animation{animation:?}\">{text}</span>")
    }
}

impl<'a> JSON<'a> {
    fn get_time(&self, message: &Message) -> String {
        let mut date = format(&message.date(&self.config.offset));
        let read_after = message.time_until_read(&self.config.offset);
        if let Some(time) = read_after {
            if !time.is_empty() {
                let who = if message.is_from_me() {
                    "them"
                } else {
                    self.config.options.custom_name.as_deref().unwrap_or("you")
                };
                date.push_str(&format!(" (Read by {who} after {time})"));
            }
        }
        date
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        env::{current_dir, set_var},
        path::PathBuf,
    };

    use crate::{
        app::attachment_manager::AttachmentManager, exporters::exporter::Writer, Config, Exporter,
        Options, JSON,
    };
    use imessage_database::{
        tables::{
            attachment::Attachment,
            messages::Message,
            table::{get_connection, ME},
        },
        util::{
            dates::get_offset, dirs::default_db_path, platform::Platform,
            query_context::QueryContext,
        },
    };

    pub(super) fn blank() -> Message {
        Message {
            rowid: i32::default(),
            guid: String::default(),
            text: None,
            service: Some("iMessage".to_string()),
            handle_id: Some(i32::default()),
            destination_caller_id: None,
            subject: None,
            date: i64::default(),
            date_read: i64::default(),
            date_delivered: i64::default(),
            is_from_me: false,
            is_read: false,
            item_type: 0,
            other_handle: 0,
            share_status: false,
            share_direction: false,
            group_title: None,
            group_action_type: 0,
            associated_message_guid: None,
            associated_message_type: Some(i32::default()),
            balloon_bundle_id: None,
            expressive_send_style_id: None,
            thread_originator_guid: None,
            thread_originator_part: None,
            date_edited: 0,
            chat_id: None,
            associated_message_emoji: None,
            num_attachments: 0,
            deleted_from: None,
            num_replies: 0,
            components: None,
            edited_parts: None,
        }
    }

    pub(super) fn fake_options() -> Options {
        Options {
            db_path: default_db_path(),
            attachment_root: None,
            attachment_manager: AttachmentManager::Disabled,
            diagnostic: false,
            export_type: None,
            export_path: PathBuf::from("/tmp"),
            query_context: QueryContext::default(),
            no_lazy: false,
            custom_name: None,
            use_caller_id: false,
            platform: Platform::macOS,
            ignore_disk_space: false,
        }
    }

    pub(super) fn fake_config(options: Options) -> Config {
        let db = get_connection(&options.get_db_path()).unwrap();
        Config {
            chatrooms: HashMap::new(),
            real_chatrooms: HashMap::new(),
            chatroom_participants: HashMap::new(),
            participants: HashMap::new(),
            real_participants: HashMap::new(),
            tapbacks: HashMap::new(),
            options,
            offset: get_offset(),
            db,
            converter: None,
        }
    }

    pub(super) fn fake_attachment() -> Attachment {
        Attachment {
            rowid: 0,
            filename: Some("a/b/c/d.jpg".to_string()),
            uti: Some("public.png".to_string()),
            mime_type: Some("image/png".to_string()),
            transfer_name: Some("d.jpg".to_string()),
            total_bytes: 100,
            is_sticker: false,
            hide_attachment: 0,
            copied_path: None,
        }
    }

    #[test]
    fn can_create() {
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();
        assert_eq!(0, exporter.files.len());
    }

    #[test]
    fn can_get_time_valid() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        // Create fake message
        let mut message = blank();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        // May 17, 2022  8:29:42 PM
        message.date_delivered = 674526582885055488;
        // May 17, 2022  9:30:31 PM
        message.date_read = 674530231992568192;

        assert_eq!(
            "May 17, 2022  5:29:42 PM (Read by you after 1 hour, 49 seconds)",
            exporter.get_time(&message)
        );
    }

    #[test]
    fn can_get_time_invalid() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        // Create fake message
        let mut message = blank();
        // May 17, 2022  9:30:31 PM
        message.date = 674530231992568192;
        // May 17, 2022  9:30:31 PM
        message.date_delivered = 674530231992568192;
        // Wed May 18 2022 02:36:24 GMT+0000
        message.date_read = 674526582885055488;
        assert_eq!("May 17, 2022  6:30:31 PM", exporter.get_time(&message));
    }

    #[test]
    fn can_format_json_from_me_normal() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some("Hello world".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);

        let expected = r#"{"rowid":0,"guid":"","text":"Hello world","service":"iMessage","handle_id":0,"destination_caller_id":null,"subject":null,"date":674526582885055488,"date_read":0,"date_delivered":0,"is_from_me":true,"is_read":false,"item_type":0,"other_handle":0,"share_status":false,"share_direction":false,"group_title":null,"group_action_type":0,"associated_message_guid":null,"associated_message_type":0,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":0,"associated_message_emoji":null,"chat_id":0,"num_attachments":0,"deleted_from":null,"num_replies":0,"components":null,"edited_parts":null}"#;
        let actual = exporter.format_message(&message, 0).unwrap();

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_message_with_html() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some("<table></table>".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);

        let expected = r#"{"rowid":0,"guid":"","text":"<table></table>","service":"iMessage","handle_id":0,"destination_caller_id":null,"subject":null,"date":674526582885055488,"date_read":0,"date_delivered":0,"is_from_me":true,"is_read":false,"item_type":0,"other_handle":0,"share_status":false,"share_direction":false,"group_title":null,"group_action_type":0,"associated_message_guid":null,"associated_message_type":0,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":0,"associated_message_emoji":null,"chat_id":0,"num_attachments":0,"deleted_from":null,"num_replies":0,"components":null,"edited_parts":null}"#;
        let actual = exporter.format_message(&message, 0).unwrap();

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_from_me_normal_deleted() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        // May 17, 2022  8:29:42 PM
        message.text = Some("Hello world".to_string());
        message.date = 674526582885055488;
        message.is_from_me = true;
        message.deleted_from = Some(0);

        let expected = r#"{"rowid":0,"guid":"","text":"Hello world","service":"iMessage","handle_id":0,"destination_caller_id":null,"subject":null,"date":674526582885055488,"date_read":0,"date_delivered":0,"is_from_me":true,"is_read":false,"item_type":0,"other_handle":0,"share_status":false,"share_direction":false,"group_title":null,"group_action_type":0,"associated_message_guid":null,"associated_message_type":0,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":0,"associated_message_emoji":null,"chat_id":null,"num_attachments":0,"deleted_from":0,"num_replies":0,"components":null,"edited_parts":null}"#;
        let actual = exporter.format_message(&message, 0).unwrap();

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_from_me_normal_read() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        message.text = Some("Hello world".to_string());
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        // May 17, 2022  9:30:31 PM
        message.date_delivered = 674530231992568192;
        message.is_from_me = true;

        let expected = r#"{"rowid":0,"guid":"","text":"Hello world","service":"iMessage","handle_id":0,"destination_caller_id":null,"subject":null,"date":674526582885055488,"date_read":0,"date_delivered":674530231992568192,"is_from_me":true,"is_read":false,"item_type":0,"other_handle":0,"share_status":false,"share_direction":false,"group_title":null,"group_action_type":0,"associated_message_guid":null,"associated_message_type":0,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":0,"associated_message_emoji":null,"chat_id":null,"num_attachments":0,"deleted_from":null,"num_replies":0,"components":null,"edited_parts":null}"#;
        let actual = exporter.format_message(&message, 0).unwrap();

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_from_them_normal() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let mut config = fake_config(options);
        config
            .participants
            .insert(999999, "Sample Contact".to_string());
        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some("Hello world".to_string());
        message.handle_id = Some(999999);

        let expected = r#"{"rowid":0,"guid":"","text":"Hello world","service":"iMessage","handle_id":999999,"destination_caller_id":null,"subject":null,"date":674526582885055488,"date_read":0,"date_delivered":0,"is_from_me":false,"is_read":false,"item_type":0,"other_handle":0,"share_status":false,"share_direction":false,"group_title":null,"group_action_type":0,"associated_message_guid":null,"associated_message_type":0,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":0,"associated_message_emoji":null,"chat_id":null,"num_attachments":0,"deleted_from":null,"num_replies":0,"components":null,"edited_parts":null}"#;
        let actual = exporter.format_message(&message, 0).unwrap();

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_from_them_normal_read() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let mut config = fake_config(options);
        config
            .participants
            .insert(999999, "Sample Contact".to_string());
        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        message.handle_id = Some(999999);
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some("Hello world".to_string());
        // May 17, 2022  8:29:42 PM
        message.date_delivered = 674526582885055488;
        // May 17, 2022  9:30:31 PM
        message.date_read = 674530231992568192;

        let expected = r#"{"rowid":0,"guid":"","text":"Hello world","service":"iMessage","handle_id":999999,"destination_caller_id":null,"subject":null,"date":674526582885055488,"date_read":674530231992568192,"date_delivered":674526582885055488,"is_from_me":false,"is_read":false,"item_type":0,"other_handle":0,"share_status":false,"share_direction":false,"group_title":null,"group_action_type":0,"associated_message_guid":null,"associated_message_type":0,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":0,"associated_message_emoji":null,"chat_id":null,"num_attachments":0,"deleted_from":null,"num_replies":0,"components":null,"edited_parts":null}"#;
        let actual = exporter.format_message(&message, 0).unwrap();

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_from_them_custom_name_read() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let mut options = fake_options();
        options.custom_name = Some("Name".to_string());
        let mut config = fake_config(options);
        config
            .participants
            .insert(999999, "Sample Contact".to_string());
        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        message.handle_id = Some(999999);
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some("Hello world".to_string());
        // May 17, 2022  8:29:42 PM
        message.date_delivered = 674526582885055488;
        // May 17, 2022  9:30:31 PM
        message.date_read = 674530231992568192;

        let expected = r#"{"rowid":0,"guid":"","text":"Hello world","service":"iMessage","handle_id":999999,"destination_caller_id":null,"subject":null,"date":674526582885055488,"date_read":674530231992568192,"date_delivered":674526582885055488,"is_from_me":false,"is_read":false,"item_type":0,"other_handle":0,"share_status":false,"share_direction":false,"group_title":null,"group_action_type":0,"associated_message_guid":null,"associated_message_type":0,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":0,"associated_message_emoji":null,"chat_id":null,"num_attachments":0,"deleted_from":null,"num_replies":0,"components":null,"edited_parts":null}"#;
        let actual = exporter.format_message(&message, 0).unwrap();

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_shareplay() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let mut config = fake_config(options);
        config.participants.insert(0, ME.to_string());

        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.item_type = 6;

        let expected = r#"{"rowid":0,"guid":"","text":null,"service":"iMessage","handle_id":0,"destination_caller_id":null,"subject":null,"date":674526582885055488,"date_read":0,"date_delivered":0,"is_from_me":false,"is_read":false,"item_type":6,"other_handle":0,"share_status":false,"share_direction":false,"group_title":null,"group_action_type":0,"associated_message_guid":null,"associated_message_type":0,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":0,"associated_message_emoji":null,"chat_id":null,"num_attachments":0,"deleted_from":null,"num_replies":0,"components":null,"edited_parts":null}"#;
        let actual = exporter.format_message(&message, 0).unwrap();

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_announcement() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let mut config = fake_config(options);
        config.participants.insert(0, ME.to_string());

        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.group_title = Some("Hello world".to_string());
        message.is_from_me = true;

        let expected = r#"{"rowid":0,"guid":"","text":null,"service":"iMessage","handle_id":0,"destination_caller_id":null,"subject":null,"date":674526582885055488,"date_read":0,"date_delivered":0,"is_from_me":true,"is_read":false,"item_type":0,"other_handle":0,"share_status":false,"share_direction":false,"group_title":"Hello world","group_action_type":0,"associated_message_guid":null,"associated_message_type":0,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":0,"associated_message_emoji":null,"chat_id":null,"num_attachments":0,"deleted_from":null,"num_replies":0,"components":null,"edited_parts":null}"#;
        let actual = exporter.format_announcement(&message);

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_announcement_custom_name() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let mut options = fake_options();
        options.custom_name = Some("Name".to_string());
        let mut config = fake_config(options);
        config.participants.insert(0, ME.to_string());

        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.group_title = Some("Hello world".to_string());

        let expected = r#"{"rowid":0,"guid":"","text":null,"service":"iMessage","handle_id":0,"destination_caller_id":null,"subject":null,"date":674526582885055488,"date_read":0,"date_delivered":0,"is_from_me":false,"is_read":false,"item_type":0,"other_handle":0,"share_status":false,"share_direction":false,"group_title":"Hello world","group_action_type":0,"associated_message_guid":null,"associated_message_type":0,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":0,"associated_message_emoji":null,"chat_id":null,"num_attachments":0,"deleted_from":null,"num_replies":0,"components":null,"edited_parts":null}"#;
        let actual = exporter.format_announcement(&message);

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_tapback_me() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let mut config = fake_config(options);
        config.participants.insert(0, ME.to_string());

        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.associated_message_type = Some(2000);
        message.associated_message_guid = Some("fake_guid".to_string());

        let expected = r#"{"rowid":0,"guid":"","text":null,"service":"iMessage","handle_id":0,"destination_caller_id":null,"subject":null,"date":674526582885055488,"date_read":0,"date_delivered":0,"is_from_me":false,"is_read":false,"item_type":0,"other_handle":0,"share_status":false,"share_direction":false,"group_title":null,"group_action_type":0,"associated_message_guid":"fake_guid","associated_message_type":2000,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":0,"associated_message_emoji":null,"chat_id":null,"num_attachments":0,"deleted_from":null,"num_replies":0,"components":null,"edited_parts":null}"#;
        let actual = exporter.format_tapback(&message).unwrap();

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_tapback_them() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let mut config = fake_config(options);
        config
            .participants
            .insert(999999, "Sample Contact".to_string());
        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.associated_message_type = Some(2000);
        message.associated_message_guid = Some("fake_guid".to_string());
        message.handle_id = Some(999999);

        let expected = r#"{"rowid":0,"guid":"","text":null,"service":"iMessage","handle_id":999999,"destination_caller_id":null,"subject":null,"date":674526582885055488,"date_read":0,"date_delivered":0,"is_from_me":false,"is_read":false,"item_type":0,"other_handle":0,"share_status":false,"share_direction":false,"group_title":null,"group_action_type":0,"associated_message_guid":"fake_guid","associated_message_type":2000,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":0,"associated_message_emoji":null,"chat_id":null,"num_attachments":0,"deleted_from":null,"num_replies":0,"components":null,"edited_parts":null}"#;
        let actual = exporter.format_tapback(&message).unwrap();

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_tapback_custom_emoji() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let mut config = fake_config(options);
        config
            .participants
            .insert(999999, "Sample Contact".to_string());
        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.associated_message_type = Some(2006);
        message.associated_message_guid = Some("fake_guid".to_string());
        message.handle_id = Some(999999);
        message.associated_message_emoji = Some("☕️".to_string());

        let expected = concat!(r#"{"rowid":0,"guid":"","text":null,"service":"iMessage","handle_id":999999,"destination_caller_id":null,"subject":null,"date":674526582885055488,"date_read":0,"date_delivered":0,"is_from_me":false,"is_read":false,"item_type":0,"other_handle":0,"share_status":false,"share_direction":false,"group_title":null,"group_action_type":0,"associated_message_guid":"fake_guid","associated_message_type":2006,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":0,"associated_message_emoji":"☕"#, "\u{fe0f}", r#"","chat_id":null,"num_attachments":0,"deleted_from":null,"num_replies":0,"components":null,"edited_parts":null}"#);
        let actual = exporter.format_tapback(&message).unwrap();

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_tapback_custom_sticker() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let mut config = fake_config(options);
        config
            .participants
            .insert(999999, "Sample Contact".to_string());
        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.associated_message_type = Some(2007);
        message.associated_message_guid = Some("fake_guid".to_string());
        message.handle_id = Some(999999);
        message.associated_message_emoji = Some("☕️".to_string());

        let expected = concat!(r#"{"rowid":0,"guid":"","text":null,"service":"iMessage","handle_id":999999,"destination_caller_id":null,"subject":null,"date":674526582885055488,"date_read":0,"date_delivered":0,"is_from_me":false,"is_read":false,"item_type":0,"other_handle":0,"share_status":false,"share_direction":false,"group_title":null,"group_action_type":0,"associated_message_guid":"fake_guid","associated_message_type":2007,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":0,"associated_message_emoji":"☕"#, "\u{fe0f}", r#"","chat_id":null,"num_attachments":0,"deleted_from":null,"num_replies":0,"components":null,"edited_parts":null}"#);
        let actual = exporter.format_tapback(&message).unwrap();

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_started_sharing_location_me() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        message.is_from_me = false;
        message.other_handle = 2;
        message.share_status = false;
        message.share_direction = false;
        message.item_type = 4;

        let expected = r#"{"rowid":0,"guid":"","text":null,"service":"iMessage","handle_id":0,"destination_caller_id":null,"subject":null,"date":0,"date_read":0,"date_delivered":0,"is_from_me":false,"is_read":false,"item_type":4,"other_handle":2,"share_status":false,"share_direction":false,"group_title":null,"group_action_type":0,"associated_message_guid":null,"associated_message_type":0,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":0,"associated_message_emoji":null,"chat_id":null,"num_attachments":0,"deleted_from":null,"num_replies":0,"components":null,"edited_parts":null}"#;
        let actual = exporter.format_message(&message, 0).unwrap();

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_stopped_sharing_location_me() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        message.is_from_me = false;
        message.other_handle = 2;
        message.share_status = true;
        message.share_direction = false;
        message.item_type = 4;

        let expected = r#"{"rowid":0,"guid":"","text":null,"service":"iMessage","handle_id":0,"destination_caller_id":null,"subject":null,"date":0,"date_read":0,"date_delivered":0,"is_from_me":false,"is_read":false,"item_type":4,"other_handle":2,"share_status":true,"share_direction":false,"group_title":null,"group_action_type":0,"associated_message_guid":null,"associated_message_type":0,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":0,"associated_message_emoji":null,"chat_id":null,"num_attachments":0,"deleted_from":null,"num_replies":0,"components":null,"edited_parts":null}"#;
        let actual = exporter.format_message(&message, 0).unwrap();

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_started_sharing_location_them() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        message.handle_id = None;
        message.is_from_me = false;
        message.other_handle = 0;
        message.share_status = false;
        message.share_direction = false;
        message.item_type = 4;

        let expected = r#"{"rowid":0,"guid":"","text":null,"service":"iMessage","handle_id":null,"destination_caller_id":null,"subject":null,"date":0,"date_read":0,"date_delivered":0,"is_from_me":false,"is_read":false,"item_type":4,"other_handle":0,"share_status":false,"share_direction":false,"group_title":null,"group_action_type":0,"associated_message_guid":null,"associated_message_type":0,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":0,"associated_message_emoji":null,"chat_id":null,"num_attachments":0,"deleted_from":null,"num_replies":0,"components":null,"edited_parts":null}"#;
        let actual = exporter.format_message(&message, 0).unwrap();

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_stopped_sharing_location_them() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        message.handle_id = None;
        message.is_from_me = false;
        message.other_handle = 0;
        message.share_status = true;
        message.share_direction = false;
        message.item_type = 4;

        let expected = r#"{"rowid":0,"guid":"","text":null,"service":"iMessage","handle_id":null,"destination_caller_id":null,"subject":null,"date":0,"date_read":0,"date_delivered":0,"is_from_me":false,"is_read":false,"item_type":4,"other_handle":0,"share_status":true,"share_direction":false,"group_title":null,"group_action_type":0,"associated_message_guid":null,"associated_message_type":0,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":0,"associated_message_emoji":null,"chat_id":null,"num_attachments":0,"deleted_from":null,"num_replies":0,"components":null,"edited_parts":null}"#;
        let actual = exporter.format_message(&message, 0).unwrap();

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_attachment_macos() {
        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let message = blank();

        let mut attachment = fake_attachment();

        let expected = r#"{"rowid":0,"filename":"a/b/c/d.jpg","uti":"public.png","mime_type":"image/png","transfer_name":"d.jpg","total_bytes":100,"is_sticker":false,"hide_attachment":0,"copied_path":null}"#;
        let actual = exporter
            .format_attachment(&mut attachment, &message)
            .unwrap();

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_attachment_macos_invalid() {
        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let message = blank();

        let mut attachment = fake_attachment();
        attachment.filename = None;

        let actual = exporter.format_attachment(&mut attachment, &message);

        assert_eq!(actual, Err("d.jpg"));
    }

    #[test]
    fn can_format_json_attachment_ios() {
        // Create exporter
        let options = fake_options();
        let mut config = fake_config(options);
        config.options.platform = Platform::iOS;
        let exporter = JSON::new(&config).unwrap();

        let message = blank();

        let mut attachment = fake_attachment();

        let expected = r#"{"rowid":0,"filename":"a/b/c/d.jpg","uti":"public.png","mime_type":"image/png","transfer_name":"d.jpg","total_bytes":100,"is_sticker":false,"hide_attachment":0,"copied_path":null}"#;
        let actual = exporter
            .format_attachment(&mut attachment, &message)
            .unwrap();

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_attachment_ios_invalid() {
        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let message = blank();

        let mut attachment = fake_attachment();
        attachment.filename = None;

        let actual = exporter.format_attachment(&mut attachment, &message);

        assert_eq!(actual, Err("d.jpg"));
    }

    #[test]
    fn can_format_json_attachment_sticker() {
        // Create exporter
        let mut options = fake_options();
        options.export_path = current_dir().unwrap().parent().unwrap().to_path_buf();

        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        // Set message to sticker variant
        message.associated_message_type = Some(1000);

        let mut attachment = fake_attachment();
        attachment.is_sticker = true;
        let sticker_path = current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("imessage-database/test_data/stickers/outline.heic");
        attachment.filename = Some(sticker_path.to_string_lossy().to_string());
        attachment.copied_path = Some(PathBuf::from(sticker_path.to_string_lossy().to_string()));

        let expected = r#"{"rowid":0,"filename":"/home/deven/git/imessage-exporter/imessage-database/test_data/stickers/outline.heic","uti":"public.png","mime_type":"image/png","transfer_name":"d.jpg","total_bytes":100,"is_sticker":true,"hide_attachment":0,"copied_path":"/home/deven/git/imessage-exporter/imessage-database/test_data/stickers/outline.heic"}"#;
        let actual = exporter.format_sticker(&mut attachment, &message);

        assert_eq!(expected, actual);

        // Remove the file created by the constructor for this test
        let orphaned_path = current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("orphaned.json");
        std::fs::remove_file(orphaned_path).unwrap();
    }
}

#[cfg(test)]
mod balloon_format_tests {
    use std::env::set_var;

    use super::tests::{blank, fake_config, fake_options};
    use crate::{exporters::exporter::BalloonFormatter, Exporter, JSON};
    use imessage_database::message_types::{
        app::AppMessage,
        app_store::AppStoreMessage,
        collaboration::CollaborationMessage,
        music::MusicMessage,
        placemark::{Placemark, PlacemarkMessage},
        url::URLMessage,
    };

    #[test]
    fn can_format_json_url() {
        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let balloon = URLMessage {
            title: Some("title"),
            summary: Some("summary"),
            url: Some("url"),
            original_url: Some("original_url"),
            item_type: Some("item_type"),
            images: vec!["images"],
            icons: vec!["icons"],
            site_name: Some("site_name"),
            placeholder: false,
        };

        let expected = r#"{"title":"title","summary":"summary","url":"url","original_url":"original_url","item_type":"item_type","images":["images"],"icons":["icons"],"site_name":"site_name","placeholder":false}"#;
        let actual = exporter.format_url(&blank(), &balloon, "");

        assert_eq!(expected, actual);
    }

    fn can_format_json_music() {
        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let balloon = MusicMessage {
            url: Some("url"),
            preview: Some("preview"),
            artist: Some("artist"),
            album: Some("album"),
            track_name: Some("track_name"),
        };

        let expected = "<div class=\"app_header\"><div class=\"name\">track_name</div><audio controls src=\"preview\" </audio></div><a href=\"url\"><div class=\"app_footer\"><div class=\"caption\">artist</div><div class=\"subcaption\">album</div></div></a>";
        let actual = exporter.format_music(&balloon, "");

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_collaboration() {
        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let balloon = CollaborationMessage {
            original_url: Some("original_url"),
            url: Some("url"),
            title: Some("title"),
            creation_date: Some(0.),
            bundle_id: Some("bundle_id"),
            app_name: Some("app_name"),
        };

        let expected = r#"{"original_url":"original_url","url":"url","title":"title","creation_date":0.0,"bundle_id":"bundle_id","app_name":"app_name"}"#;
        let actual = exporter.format_collaboration(&balloon, "");

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_apple_pay() {
        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let balloon = AppMessage {
            image: Some("image"),
            url: Some("url"),
            title: Some("title"),
            subtitle: Some("subtitle"),
            caption: Some("caption"),
            subcaption: Some("subcaption"),
            trailing_caption: Some("trailing_caption"),
            trailing_subcaption: Some("trailing_subcaption"),
            app_name: Some("app_name"),
            ldtext: Some("ldtext"),
        };

        let expected = r#"{"image":"image","url":"url","title":"title","subtitle":"subtitle","caption":"caption","subcaption":"subcaption","trailing_caption":"trailing_caption","trailing_subcaption":"trailing_subcaption","app_name":"app_name","ldtext":"ldtext"}"#;
        let actual = exporter.format_apple_pay(&balloon, "");

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_fitness() {
        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let balloon = AppMessage {
            image: Some("image"),
            url: Some("url"),
            title: Some("title"),
            subtitle: Some("subtitle"),
            caption: Some("caption"),
            subcaption: Some("subcaption"),
            trailing_caption: Some("trailing_caption"),
            trailing_subcaption: Some("trailing_subcaption"),
            app_name: Some("app_name"),
            ldtext: Some("ldtext"),
        };

        let expected = r#"{"image":"image","url":"url","title":"title","subtitle":"subtitle","caption":"caption","subcaption":"subcaption","trailing_caption":"trailing_caption","trailing_subcaption":"trailing_subcaption","app_name":"app_name","ldtext":"ldtext"}"#;
        let actual = exporter.format_fitness(&balloon, "");

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_slideshow() {
        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let balloon = AppMessage {
            image: Some("image"),
            url: Some("url"),
            title: Some("title"),
            subtitle: Some("subtitle"),
            caption: Some("caption"),
            subcaption: Some("subcaption"),
            trailing_caption: Some("trailing_caption"),
            trailing_subcaption: Some("trailing_subcaption"),
            app_name: Some("app_name"),
            ldtext: Some("ldtext"),
        };

        let expected = r#"{"image":"image","url":"url","title":"title","subtitle":"subtitle","caption":"caption","subcaption":"subcaption","trailing_caption":"trailing_caption","trailing_subcaption":"trailing_subcaption","app_name":"app_name","ldtext":"ldtext"}"#;
        let actual = exporter.format_slideshow(&balloon, "");

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_find_my() {
        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let balloon = AppMessage {
            image: Some("image"),
            url: Some("url"),
            title: Some("title"),
            subtitle: Some("subtitle"),
            caption: Some("caption"),
            subcaption: Some("subcaption"),
            trailing_caption: Some("trailing_caption"),
            trailing_subcaption: Some("trailing_subcaption"),
            app_name: Some("app_name"),
            ldtext: Some("ldtext"),
        };

        let expected = r#"{"image":"image","url":"url","title":"title","subtitle":"subtitle","caption":"caption","subcaption":"subcaption","trailing_caption":"trailing_caption","trailing_subcaption":"trailing_subcaption","app_name":"app_name","ldtext":"ldtext"}"#;
        let actual = exporter.format_find_my(&balloon, "");

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_check_in_timer() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let balloon = AppMessage {
            image: None,
            url: Some("?messageType=1&interfaceVersion=1&sendDate=1697316869.688709"),
            title: None,
            subtitle: None,
            caption: Some("Check In: Timer Started"),
            subcaption: None,
            trailing_caption: None,
            trailing_subcaption: None,
            app_name: Some("Check In"),
            ldtext: Some("Check In: Timer Started"),
        };

        let expected = concat!(r#"{"image":null,"url":"?messageType=1&interfaceVersion=1&sendDate=1697316869.688709","title":null,"subtitle":null,"caption":"Check"#, "\u{a0}", r#"In: Timer Started","subcaption":null,"trailing_caption":null,"trailing_subcaption":null,"app_name":"Check"#, "\u{a0}", r#"In","ldtext":"Check"#, "\u{a0}", r#"In: Timer Started"}"#);
        let actual = exporter.format_check_in(&balloon, "");

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_check_in_timer_late() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let balloon = AppMessage {
            image: None,
            url: Some("?messageType=1&interfaceVersion=1&sendDate=1697316869.688709"),
            title: None,
            subtitle: None,
            caption: Some("Check In: Has not checked in when expected, location shared"),
            subcaption: None,
            trailing_caption: None,
            trailing_subcaption: None,
            app_name: Some("Check In"),
            ldtext: Some("Check In: Has not checked in when expected, location shared"),
        };

        let expected = concat!(r#"{"image":null,"url":"?messageType=1&interfaceVersion=1&sendDate=1697316869.688709","title":null,"subtitle":null,"caption":"Check"#, "\u{a0}", r#"In: Has not checked in when expected, location shared","subcaption":null,"trailing_caption":null,"trailing_subcaption":null,"app_name":"Check"#, "\u{a0}", r#"In","ldtext":"Check"#, "\u{a0}", r#"In: Has not checked in when expected, location shared"}"#);
        let actual = exporter.format_check_in(&balloon, "");

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_accepted_check_in() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let balloon = AppMessage {
            image: None,
            url: Some("?messageType=1&interfaceVersion=1&sendDate=1697316869.688709"),
            title: None,
            subtitle: None,
            caption: Some("Check In: Fake Location"),
            subcaption: None,
            trailing_caption: None,
            trailing_subcaption: None,
            app_name: Some("Check In"),
            ldtext: Some("Check In: Fake Location"),
        };

        let expected = concat!(r#"{"image":null,"url":"?messageType=1&interfaceVersion=1&sendDate=1697316869.688709","title":null,"subtitle":null,"caption":"Check"#, "\u{a0}", r#"In: Fake Location","subcaption":null,"trailing_caption":null,"trailing_subcaption":null,"app_name":"Check"#, "\u{a0}", r#"In","ldtext":"Check"#, "\u{a0}", r#"In: Fake Location"}"#);
        let actual = exporter.format_check_in(&balloon, "");

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_app_store() {
        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let balloon = AppStoreMessage {
            url: Some("url"),
            app_name: Some("app_name"),
            original_url: Some("original_url"),
            description: Some("description"),
            platform: Some("platform"),
            genre: Some("genre"),
        };

        let expected = r#"{"url":"url","original_url":"original_url","app_name":"app_name","description":"description","platform":"platform","genre":"genre"}"#;
        let actual = exporter.format_app_store(&balloon, "");

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_placemark() {
        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let balloon = PlacemarkMessage {
            url: Some("url"),
            original_url: Some("original_url"),
            place_name: Some("Name"),
            placemark: Placemark {
                name: Some("name"),
                address: Some("address"),
                state: Some("state"),
                city: Some("city"),
                iso_country_code: Some("iso_country_code"),
                postal_code: Some("postal_code"),
                country: Some("country"),
                street: Some("street"),
                sub_administrative_area: Some("sub_administrative_area"),
                sub_locality: Some("sub_locality"),
            },
        };

        let expected = r#"{"url":"url","original_url":"original_url","place_name":"Name","placemark":{"name":"name","address":"address","state":"state","city":"city","iso_country_code":"iso_country_code","postal_code":"postal_code","country":"country","street":"street","sub_administrative_area":"sub_administrative_area","sub_locality":"sub_locality"}}"#;
        let actual = exporter.format_placemark(&balloon, "");

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_generic_app() {
        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let balloon = AppMessage {
            image: Some("image"),
            url: Some("url"),
            title: Some("title"),
            subtitle: Some("subtitle"),
            caption: Some("caption"),
            subcaption: Some("subcaption"),
            trailing_caption: Some("trailing_caption"),
            trailing_subcaption: Some("trailing_subcaption"),
            app_name: Some("app_name"),
            ldtext: Some("ldtext"),
        };

        let expected = r#"{"image":"image","url":"url","title":"title","subtitle":"subtitle","caption":"caption","subcaption":"subcaption","trailing_caption":"trailing_caption","trailing_subcaption":"trailing_subcaption","app_name":"app_name","ldtext":"ldtext"}"#;
        let actual = exporter.format_generic_app(&balloon, "bundle_id", &mut vec![], "");

        assert_eq!(expected, actual);
    }
}

#[cfg(test)]
mod text_effect_tests {
    use super::tests::{blank, fake_config, fake_options};
    use crate::{
        exporters::exporter::{TextEffectFormatter, Writer},
        Exporter, JSON,
    };
    use imessage_database::{
        message_types::text_effects::{Style, TextEffect, Unit},
        util::typedstream::parser::TypedStreamReader,
    };
    use std::{
        env::{current_dir, set_var},
        fs::File,
        io::Read,
    };

    #[test]
    fn can_format_json_default() {
        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let expected = r#"{ text: "Chris", text_effect: "Default" }"#;
        let actual = exporter.format_attributed("Chris", &TextEffect::Default);

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_mention() {
        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let expected = "<span title=\"+15558675309\"><b>Chris</b></span>";
        let actual = exporter.format_mention("Chris", "+15558675309");

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_link() {
        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let expected = "<a href=\"https://chrissardegna.com\">chrissardegna.com</a>";
        let actual = exporter.format_link("chrissardegna.com", "https://chrissardegna.com");

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_otp() {
        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let expected = "<u>123456</u>";
        let actual = exporter.format_otp("123456");

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_style_single() {
        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let expected = "<b>Bold</b>";
        let actual = exporter.format_styles("Bold", &[Style::Bold]);

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_style_multiple() {
        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let expected = "<s><b>Bold</b></s>";
        let actual = exporter.format_styles("Bold", &[Style::Bold, Style::Strikethrough]);

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_style_all() {
        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let expected = "<u><i><s><b>Bold</b></s></i></u>";
        let actual = exporter.format_styles(
            "Bold",
            &[
                Style::Bold,
                Style::Strikethrough,
                Style::Italic,
                Style::Underline,
            ],
        );

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_conversion() {
        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let expected = "<u>100 Miles</u>";
        let actual = exporter.format_conversion("100 Miles", &Unit::Distance);

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_mention_end_to_end() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some("Test Dad ".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);

        let typedstream_path = current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("imessage-database/test_data/typedstream/Mention");
        let mut file = File::open(typedstream_path).unwrap();
        let mut bytes = vec![];
        file.read_to_end(&mut bytes).unwrap();

        let mut parser = TypedStreamReader::from(&bytes);
        message.components = parser.parse().ok();

        let expected = exporter.format_message(&message, 0).unwrap();
        let actual = r#"{"rowid":0,"guid":"","text":"Test Dad ","service":"iMessage","handle_id":0,"destination_caller_id":null,"subject":null,"date":674526582885055488,"date_read":0,"date_delivered":0,"is_from_me":true,"is_read":false,"item_type":0,"other_handle":0,"share_status":false,"share_direction":false,"group_title":null,"group_action_type":0,"associated_message_guid":null,"associated_message_type":0,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":0,"associated_message_emoji":null,"chat_id":0,"num_attachments":0,"deleted_from":null,"num_replies":0,"components":[{"Object":[{"name":"NSMutableString","version":1},[{"String":"Test Dad "}]]},{"Data":[{"SignedInteger":1},{"UnsignedInteger":5}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":1}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":0}]]},{"Data":[{"SignedInteger":2},{"UnsignedInteger":3}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":2}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMentionConfirmedMention"}]]},{"Object":[{"name":"NSString","version":1},[{"String":"+15558675309"}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":0}]]},{"Data":[{"SignedInteger":1},{"UnsignedInteger":1}]}],"edited_parts":null}"#;

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_otp_end_to_end() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some("000123 is your security code. Don't share your code.".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);

        let typedstream_path = current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("imessage-database/test_data/typedstream/Code");
        let mut file = File::open(typedstream_path).unwrap();
        let mut bytes = vec![];
        file.read_to_end(&mut bytes).unwrap();

        let mut parser = TypedStreamReader::from(&bytes);
        message.components = parser.parse().ok();

        let expected = r#"{"rowid":0,"guid":"","text":"000123 is your security code. Don't share your code.","service":"iMessage","handle_id":0,"destination_caller_id":null,"subject":null,"date":674526582885055488,"date_read":0,"date_delivered":0,"is_from_me":true,"is_read":false,"item_type":0,"other_handle":0,"share_status":false,"share_direction":false,"group_title":null,"group_action_type":0,"associated_message_guid":null,"associated_message_type":0,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":0,"associated_message_emoji":null,"chat_id":0,"num_attachments":0,"deleted_from":null,"num_replies":0,"components":[{"Object":[{"name":"NSMutableString","version":1},[{"String":"000123 is your security code. Don't share your code."}]]},{"Data":[{"SignedInteger":1},{"UnsignedInteger":6}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":3}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":0}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMOneTimeCodeAttributeName"}]]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":2}]]},{"Object":[{"name":"NSString","version":1},[{"String":"displayCode"}]]},{"Object":[{"name":"NSString","version":1},[{"String":"000123"}]]},{"Object":[{"name":"NSString","version":1},[{"String":"code"}]]},{"Object":[{"name":"NSString","version":1},[{"String":"000123"}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMDataDetectedAttributeName"}]]},{"Object":[{"name":"NSData","version":0},[{"SignedInteger":535}]]},{"Data":[{"Array":[98,112,108,105,115,116,48,48,212,1,2,3,4,5,6,7,12,88,36,118,101,114,115,105,111,110,89,36,97,114,99,104,105,118,101,114,84,36,116,111,112,88,36,111,98,106,101,99,116,115,18,0,1,134,160,95,16,15,78,83,75,101,121,101,100,65,114,99,104,105,118,101,114,210,8,9,10,11,87,118,101,114,115,105,111,110,89,100,100,45,114,101,115,117,108,116,128,11,128,1,172,13,14,28,36,37,38,44,45,46,50,53,57,85,36,110,117,108,108,215,15,16,17,18,19,20,21,22,23,24,25,26,27,26,82,77,83,86,36,99,108,97,115,115,82,65,82,81,84,81,80,82,83,82,82,86,78,128,6,128,10,128,2,128,7,16,1,128,8,212,29,30,31,16,32,33,34,35,95,16,18,78,83,46,114,97,110,103,101,118,97,108,46,108,101,110,103,116,104,95,16,20,78,83,46,114,97,110,103,101,118,97,108,46,108,111,99,97,116,105,111,110,90,78,83,46,115,112,101,99,105,97,108,128,3,128,4,16,4,128,5,16,6,16,0,210,39,40,41,42,90,36,99,108,97,115,115,110,97,109,101,88,36,99,108,97,115,115,101,115,87,78,83,86,97,108,117,101,162,41,43,88,78,83,79,98,106,101,99,116,86,48,48,48,49,50,51,88,65,117,116,104,67,111,100,101,210,47,16,48,49,90,78,83,46,111,98,106,101,99,116,115,160,128,9,210,39,40,51,52,87,78,83,65,114,114,97,121,162,51,43,210,39,40,54,55,95,16,15,68,68,83,99,97,110,110,101,114,82,101,115,117,108,116,162,56,43,95,16,15,68,68,83,99,97,110,110,101,114,82,101,115,117,108,116,16,1,0,8,0,17,0,26,0,36,0,41,0,50,0,55,0,73,0,78,0,86,0,96,0,98,0,100,0,113,0,119,0,134,0,137,0,144,0,147,0,149,0,151,0,154,0,157,0,159,0,161,0,163,0,165,0,167,0,169,0,178,0,199,0,222,0,233,0,235,0,237,0,239,0,241,0,243,0,245,0,250,1,5,1,14,1,22,1,25,1,34,1,41,1,50,1,55,1,66,1,67,1,69,1,74,1,82,1,85,1,90,1,108,1,111,1,129,0,0,0,0,0,0,2,1,0,0,0,0,0,0,0,58,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1,131]}]},{"Data":[{"SignedInteger":2},{"UnsignedInteger":46}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":1}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":0}]]}],"edited_parts":null}"#;
        let actual = exporter.format_message(&message, 0).unwrap();

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_link_end_to_end() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some("https://twitter.com/xxxxxxxxx/status/0000223300009216128".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);

        let typedstream_path = current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("imessage-database/test_data/typedstream/URLMessage");
        let mut file = File::open(typedstream_path).unwrap();
        let mut bytes = vec![];
        file.read_to_end(&mut bytes).unwrap();

        let mut parser = TypedStreamReader::from(&bytes);
        message.components = parser.parse().ok();

        let expected = r#"{"rowid":0,"guid":"","text":"https://twitter.com/xxxxxxxxx/status/0000223300009216128","service":"iMessage","handle_id":0,"destination_caller_id":null,"subject":null,"date":674526582885055488,"date_read":0,"date_delivered":0,"is_from_me":true,"is_read":false,"item_type":0,"other_handle":0,"share_status":false,"share_direction":false,"group_title":null,"group_action_type":0,"associated_message_guid":null,"associated_message_type":0,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":0,"associated_message_emoji":null,"chat_id":0,"num_attachments":0,"deleted_from":null,"num_replies":0,"components":[{"Object":[{"name":"NSMutableString","version":1},[{"String":"https://twitter.com/xxxxxxxxx/status/0000223300009216128"}]]},{"Data":[{"SignedInteger":1},{"UnsignedInteger":56}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":4}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMLinkAttributeName"}]]},{"Object":[{"name":"NSURL","version":0},[{"SignedInteger":0}]]},{"Object":[{"name":"NSString","version":1},[{"String":"https://twitter.com/xxxxxxxxx/status/0000223300009216128"}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":0}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMDataDetectedAttributeName"}]]},{"Object":[{"name":"NSMutableData","version":0},[{"SignedInteger":604}]]},{"Data":[{"Array":[98,112,108,105,115,116,48,48,212,1,2,3,4,5,6,7,12,88,36,118,101,114,115,105,111,110,89,36,97,114,99,104,105,118,101,114,84,36,116,111,112,88,36,111,98,106,101,99,116,115,18,0,1,134,160,95,16,15,78,83,75,101,121,101,100,65,114,99,104,105,118,101,114,210,8,9,13,11,87,118,101,114,115,105,111,110,89,100,100,45,114,101,115,117,108,116,128,11,128,1,172,13,14,28,36,37,38,44,45,46,50,54,58,85,36,110,117,108,108,215,15,16,17,18,19,20,21,22,23,24,25,26,27,26,82,77,83,86,36,99,108,97,115,115,82,65,82,81,84,81,80,82,83,82,82,86,78,128,6,128,13,128,2,128,7,16,1,128,8,212,29,30,31,16,32,33,34,35,95,16,18,78,83,46,114,97,110,103,101,118,97,108,46,108,101,110,103,116,104,95,16,20,78,83,46,114,97,110,103,101,118,97,108,46,108,111,99,97,116,105,111,110,90,78,83,46,115,112,101,99,105,97,108,128,3,128,4,16,4,128,5,16,56,16,0,210,39,40,41,42,90,36,99,108,97,115,115,110,97,109,101,88,36,99,108,97,115,115,101,115,87,78,83,86,97,108,117,101,162,41,43,88,78,83,79,98,106,101,99,116,95,16,56,104,116,116,112,115,58,47,47,116,119,105,116,116,101,114,46,99,111,109,47,120,120,120,120,120,120,120,120,120,47,115,116,97,116,117,115,47,48,48,48,48,50,50,51,51,48,48,48,48,57,50,49,54,49,50,56,87,72,116,116,112,85,82,76,210,47,16,48,49,90,78,83,46,111,98,106,101,99,116,115,160,128,9,210,39,40,51,52,94,78,83,77,117,116,97,98,108,101,65,114,114,97,121,163,51,53,43,87,78,83,65,114,114,97,121,210,39,40,55,56,95,16,15,68,68,83,99,97,110,110,101,114,82,101,115,117,108,116,162,57,43,95,16,15,68,68,83,99,97,110,110,101,114,82,101,115,117,108,116,16,1,0,8,0,17,0,26,0,36,0,41,0,50,0,55,0,73,0,78,0,86,0,96,0,98,0,100,0,113,0,119,0,134,0,137,0,144,0,147,0,149,0,151,0,154,0,157,0,159,0,161,0,163,0,165,0,167,0,169,0,178,0,199,0,222,0,233,0,235,0,237,0,239,0,241,0,243,0,245,0,250,1,5,1,14,1,22,1,25,1,34,1,93,1,101,1,106,1,117,1,118,1,120,1,125,1,140,1,144,1,152,1,157,1,175,1,178,1,196,0,0,0,0,0,0,2,1,0,0,0,0,0,0,0,59,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1,198]}]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMBaseWritingDirectionAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":-1}]]}],"edited_parts":null}"#;
        let actual = exporter.format_message(&message, 0).unwrap();

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_conversion_end_to_end() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some("Hi. Right now or tomorrow?".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);

        let typedstream_path = current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("imessage-database/test_data/typedstream/Date");
        let mut file = File::open(typedstream_path).unwrap();
        let mut bytes = vec![];
        file.read_to_end(&mut bytes).unwrap();

        let mut parser = TypedStreamReader::from(&bytes);
        message.components = parser.parse().ok();

        let expected = r#"{"rowid":0,"guid":"","text":"Hi. Right now or tomorrow?","service":"iMessage","handle_id":0,"destination_caller_id":null,"subject":null,"date":674526582885055488,"date_read":0,"date_delivered":0,"is_from_me":true,"is_read":false,"item_type":0,"other_handle":0,"share_status":false,"share_direction":false,"group_title":null,"group_action_type":0,"associated_message_guid":null,"associated_message_type":0,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":0,"associated_message_emoji":null,"chat_id":0,"num_attachments":0,"deleted_from":null,"num_replies":0,"components":[{"Object":[{"name":"NSMutableString","version":1},[{"String":"Hi. Right now or tomorrow?"}]]},{"Data":[{"SignedInteger":1},{"UnsignedInteger":17}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":1}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":0}]]},{"Data":[{"SignedInteger":2},{"UnsignedInteger":8}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":2}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMCalendarEventAttributeName"}]]},{"Object":[{"name":"NSMutableData","version":0},[{"SignedInteger":669}]]},{"Data":[{"Array":[98,112,108,105,115,116,48,48,212,1,2,3,4,5,6,7,12,88,36,118,101,114,115,105,111,110,89,36,97,114,99,104,105,118,101,114,84,36,116,111,112,88,36,111,98,106,101,99,116,115,18,0,1,134,160,95,16,15,78,83,75,101,121,101,100,65,114,99,104,105,118,101,114,210,8,9,10,11,87,118,101,114,115,105,111,110,89,100,100,45,114,101,115,117,108,116,128,16,128,1,175,16,17,13,14,29,37,38,39,45,46,47,52,60,64,65,68,72,73,77,85,36,110,117,108,108,215,15,16,17,18,19,20,21,22,23,24,25,26,27,28,82,77,83,86,36,99,108,97,115,115,82,65,82,81,84,81,80,82,83,82,82,86,78,128,6,128,15,128,2,128,7,16,0,128,8,16,1,212,30,31,32,16,33,34,35,36,95,16,18,78,83,46,114,97,110,103,101,118,97,108,46,108,101,110,103,116,104,95,16,20,78,83,46,114,97,110,103,101,118,97,108,46,108,111,99,97,116,105,111,110,90,78,83,46,115,112,101,99,105,97,108,128,3,128,4,16,4,128,5,16,8,16,17,210,40,41,42,43,90,36,99,108,97,115,115,110,97,109,101,88,36,99,108,97,115,115,101,115,87,78,83,86,97,108,117,101,162,42,44,88,78,83,79,98,106,101,99,116,88,116,111,109,111,114,114,111,119,84,68,97,116,101,210,48,16,49,51,90,78,83,46,111,98,106,101,99,116,115,161,50,128,9,128,13,216,15,16,53,17,18,19,20,21,22,23,56,57,58,28,59,28,81,86,128,6,128,15,128,14,128,10,128,11,128,12,212,30,31,32,16,33,34,35,36,128,3,128,4,128,5,91,82,101,108,97,116,105,118,101,68,97,121,210,48,16,66,51,160,128,13,210,40,41,69,70,94,78,83,77,117,116,97,98,108,101,65,114,114,97,121,163,69,71,44,87,78,83,65,114,114,97,121,81,49,210,40,41,74,75,95,16,15,68,68,83,99,97,110,110,101,114,82,101,115,117,108,116,162,76,44,95,16,15,68,68,83,99,97,110,110,101,114,82,101,115,117,108,116,16,1,0,8,0,17,0,26,0,36,0,41,0,50,0,55,0,73,0,78,0,86,0,96,0,98,0,100,0,120,0,126,0,141,0,144,0,151,0,154,0,156,0,158,0,161,0,164,0,166,0,168,0,170,0,172,0,174,0,176,0,178,0,187,0,208,0,231,0,242,0,244,0,246,0,248,0,250,0,252,0,254,1,3,1,14,1,23,1,31,1,34,1,43,1,52,1,57,1,62,1,73,1,75,1,77,1,79,1,96,1,98,1,100,1,102,1,104,1,106,1,108,1,110,1,119,1,121,1,123,1,125,1,137,1,142,1,143,1,145,1,150,1,165,1,169,1,177,1,179,1,184,1,202,1,205,1,223,0,0,0,0,0,0,2,1,0,0,0,0,0,0,0,78,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1,225]}]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":0}]]},{"Data":[{"SignedInteger":1},{"UnsignedInteger":1}]}],"edited_parts":null}"#;
        let actual = exporter.format_message(&message, 0).unwrap();

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_text_effect_end_to_end() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some("Big small shake nod explode ripple bloom jitter".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);

        let typedstream_path = current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("imessage-database/test_data/typedstream/TextEffects");
        let mut file = File::open(typedstream_path).unwrap();
        let mut bytes = vec![];
        file.read_to_end(&mut bytes).unwrap();

        let mut parser = TypedStreamReader::from(&bytes);
        message.components = parser.parse().ok();

        let expected = r#"{"rowid":0,"guid":"","text":"Big small shake nod explode ripple bloom jitter","service":"iMessage","handle_id":0,"destination_caller_id":null,"subject":null,"date":674526582885055488,"date_read":0,"date_delivered":0,"is_from_me":true,"is_read":false,"item_type":0,"other_handle":0,"share_status":false,"share_direction":false,"group_title":null,"group_action_type":0,"associated_message_guid":null,"associated_message_type":0,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":0,"associated_message_emoji":null,"chat_id":0,"num_attachments":0,"deleted_from":null,"num_replies":0,"components":[{"Object":[{"name":"NSString","version":1},[{"String":"Big small shake nod explode ripple bloom jitter"}]]},{"Data":[{"SignedInteger":1},{"UnsignedInteger":3}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":2}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMTextEffectAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":5}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":0}]]},{"Data":[{"SignedInteger":2},{"UnsignedInteger":1}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":1}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":0}]]},{"Data":[{"SignedInteger":3},{"UnsignedInteger":6}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":2}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMTextEffectAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":11}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":0}]]},{"Data":[{"SignedInteger":4},{"UnsignedInteger":5}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":2}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMTextEffectAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":9}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":0}]]},{"Data":[{"SignedInteger":3},{"UnsignedInteger":1}]},{"Data":[{"SignedInteger":5},{"UnsignedInteger":3}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":2}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMTextEffectAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":8}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":0}]]},{"Data":[{"SignedInteger":3},{"UnsignedInteger":1}]},{"Data":[{"SignedInteger":6},{"UnsignedInteger":8}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":2}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMTextEffectAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":12}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":0}]]},{"Data":[{"SignedInteger":7},{"UnsignedInteger":6}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":2}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMTextEffectAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":4}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":0}]]},{"Data":[{"SignedInteger":6},{"UnsignedInteger":1}]},{"Data":[{"SignedInteger":8},{"UnsignedInteger":5}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":2}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMTextEffectAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":6}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":0}]]},{"Data":[{"SignedInteger":6},{"UnsignedInteger":1}]},{"Data":[{"SignedInteger":9},{"UnsignedInteger":6}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":2}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMTextEffectAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":10}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":0}]]}],"edited_parts":null}"#;
        let actual = exporter.format_message(&message, 0).unwrap();

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_text_styles_end_to_end() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some("Bold underline italic strikethrough all four".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);

        let typedstream_path = current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("imessage-database/test_data/typedstream/TextStyles");
        let mut file = File::open(typedstream_path).unwrap();
        let mut bytes = vec![];
        file.read_to_end(&mut bytes).unwrap();

        let mut parser = TypedStreamReader::from(&bytes);
        message.components = parser.parse().ok();

        let expected = r#"{"rowid":0,"guid":"","text":"Bold underline italic strikethrough all four","service":"iMessage","handle_id":0,"destination_caller_id":null,"subject":null,"date":674526582885055488,"date_read":0,"date_delivered":0,"is_from_me":true,"is_read":false,"item_type":0,"other_handle":0,"share_status":false,"share_direction":false,"group_title":null,"group_action_type":0,"associated_message_guid":null,"associated_message_type":0,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":0,"associated_message_emoji":null,"chat_id":0,"num_attachments":0,"deleted_from":null,"num_replies":0,"components":[{"Object":[{"name":"NSString","version":1},[{"String":"Bold underline italic strikethrough all four"}]]},{"Data":[{"SignedInteger":1},{"UnsignedInteger":4}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":2}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMTextBoldAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":1}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":0}]]},{"Data":[{"SignedInteger":2},{"UnsignedInteger":1}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":1}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":0}]]},{"Data":[{"SignedInteger":3},{"UnsignedInteger":9}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":2}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMTextUnderlineAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":1}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":0}]]},{"Data":[{"SignedInteger":2},{"UnsignedInteger":1}]},{"Data":[{"SignedInteger":4},{"UnsignedInteger":6}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":2}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMTextItalicAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":1}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":0}]]},{"Data":[{"SignedInteger":2},{"UnsignedInteger":1}]},{"Data":[{"SignedInteger":5},{"UnsignedInteger":13}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":2}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMTextStrikethroughAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":1}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":0}]]},{"Data":[{"SignedInteger":2},{"UnsignedInteger":5}]},{"Data":[{"SignedInteger":6},{"UnsignedInteger":4}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":5}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMTextBoldAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":1}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMTextStrikethroughAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":1}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":0}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMTextUnderlineAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":1}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMTextItalicAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":1}]]}],"edited_parts":null}"#;
        let actual = exporter.format_message(&message, 0).unwrap();

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_text_styles_single_end_to_end() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some("Everything".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);

        let typedstream_path = current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("imessage-database/test_data/typedstream/TextStylesSingleRange");
        let mut file = File::open(typedstream_path).unwrap();
        let mut bytes = vec![];
        file.read_to_end(&mut bytes).unwrap();

        let mut parser = TypedStreamReader::from(&bytes);
        message.components = parser.parse().ok();

        let expected = r#"{"rowid":0,"guid":"","text":"Everything","service":"iMessage","handle_id":0,"destination_caller_id":null,"subject":null,"date":674526582885055488,"date_read":0,"date_delivered":0,"is_from_me":true,"is_read":false,"item_type":0,"other_handle":0,"share_status":false,"share_direction":false,"group_title":null,"group_action_type":0,"associated_message_guid":null,"associated_message_type":0,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":0,"associated_message_emoji":null,"chat_id":0,"num_attachments":0,"deleted_from":null,"num_replies":0,"components":[{"Object":[{"name":"NSString","version":1},[{"String":"Everything"}]]},{"Data":[{"SignedInteger":1},{"UnsignedInteger":10}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":5}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMTextBoldAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":1}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMTextStrikethroughAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":1}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":0}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMTextUnderlineAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":1}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMTextItalicAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":1}]]}],"edited_parts":null}"#;
        let actual = exporter.format_message(&message, 0).unwrap();

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_text_styles_mixed_end_to_end() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some("Underline normal jitter normal".to_string());
        message.is_from_me = true;
        message.chat_id = Some(0);

        let typedstream_path = current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("imessage-database/test_data/typedstream/TextStylesMixed");
        let mut file = File::open(typedstream_path).unwrap();
        let mut bytes = vec![];
        file.read_to_end(&mut bytes).unwrap();

        let mut parser = TypedStreamReader::from(&bytes);
        message.components = parser.parse().ok();

        let expected = r#"{"rowid":0,"guid":"","text":"Underline normal jitter normal","service":"iMessage","handle_id":0,"destination_caller_id":null,"subject":null,"date":674526582885055488,"date_read":0,"date_delivered":0,"is_from_me":true,"is_read":false,"item_type":0,"other_handle":0,"share_status":false,"share_direction":false,"group_title":null,"group_action_type":0,"associated_message_guid":null,"associated_message_type":0,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":0,"associated_message_emoji":null,"chat_id":0,"num_attachments":0,"deleted_from":null,"num_replies":0,"components":[{"Object":[{"name":"NSString","version":1},[{"String":"Underline normal jitter normal"}]]},{"Data":[{"SignedInteger":1},{"UnsignedInteger":9}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":3}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMBaseWritingDirectionAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":-1}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":0}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMTextUnderlineAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":1}]]},{"Data":[{"SignedInteger":2},{"UnsignedInteger":8}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":2}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMBaseWritingDirectionAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":-1}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":0}]]},{"Data":[{"SignedInteger":3},{"UnsignedInteger":6}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":3}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMBaseWritingDirectionAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":-1}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":0}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMTextEffectAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":10}]]},{"Data":[{"SignedInteger":2},{"UnsignedInteger":7}]}],"edited_parts":null}"#;
        let actual = exporter.format_message(&message, 0).unwrap();

        assert_eq!(expected, actual);
    }
}

#[cfg(test)]
mod edited_tests {
    use std::{
        env::{current_dir, set_var},
        fs::File,
        io::Read,
    };

    use super::tests::{blank, fake_config, fake_options};

    use crate::{exporters::exporter::Writer, Exporter, JSON};
    use imessage_database::{
        message_types::edited::{EditStatus, EditedMessage, EditedMessagePart},
        util::typedstream::parser::TypedStreamReader,
    };

    #[test]
    fn can_format_json_conversion_final_unsent() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.date_edited = 674530231992568192;
        message.text = Some(
            "From arbitrary byte stream:\r\u{FFFC}To native Rust data structures:\r".to_string(),
        );
        message.is_from_me = true;
        message.chat_id = Some(0);
        message.edited_parts = Some(EditedMessage {
            parts: vec![
                EditedMessagePart {
                    status: EditStatus::Original,
                    edit_history: vec![],
                },
                EditedMessagePart {
                    status: EditStatus::Original,
                    edit_history: vec![],
                },
                EditedMessagePart {
                    status: EditStatus::Original,
                    edit_history: vec![],
                },
                EditedMessagePart {
                    status: EditStatus::Unsent,
                    edit_history: vec![],
                },
            ],
        });

        let typedstream_path = current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("imessage-database/test_data/typedstream/MultiPartWithDeleted");
        let mut file = File::open(typedstream_path).unwrap();
        let mut bytes = vec![];
        file.read_to_end(&mut bytes).unwrap();

        let mut parser = TypedStreamReader::from(&bytes);
        message.components = parser.parse().ok();

        let expected = r#"{"rowid":0,"guid":"","text":"From arbitrary byte stream:\r￼To native Rust data structures:\r","service":"iMessage","handle_id":0,"destination_caller_id":null,"subject":null,"date":674526582885055488,"date_read":0,"date_delivered":0,"is_from_me":true,"is_read":false,"item_type":0,"other_handle":0,"share_status":false,"share_direction":false,"group_title":null,"group_action_type":0,"associated_message_guid":null,"associated_message_type":0,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":674530231992568192,"associated_message_emoji":null,"chat_id":0,"num_attachments":0,"deleted_from":null,"num_replies":0,"components":[{"Object":[{"name":"NSMutableString","version":1},[{"String":"From arbitrary byte stream:\r￼To native Rust data structures:\r"}]]},{"Data":[{"SignedInteger":1},{"UnsignedInteger":28}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":1}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":0}]]},{"Data":[{"SignedInteger":2},{"UnsignedInteger":1}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":2}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMFileTransferGUIDAttributeName"}]]},{"Object":[{"name":"NSString","version":1},[{"String":"D0551D89-4E11-43D0-9A0E-06F19704E97B"}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":1}]]},{"Data":[{"SignedInteger":3},{"UnsignedInteger":32}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":1}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":2}]]}],"edited_parts":{"parts":[{"status":"Original","edit_history":[]},{"status":"Original","edit_history":[]},{"status":"Original","edit_history":[]},{"status":"Unsent","edit_history":[]}]}}"#;
        let actual = exporter.format_message(&message, 0).unwrap();

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_conversion_no_edits() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.text = Some(
            "From arbitrary byte stream:\r\u{FFFC}To native Rust data structures:\r".to_string(),
        );
        message.is_from_me = true;
        message.chat_id = Some(0);

        let typedstream_path = current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("imessage-database/test_data/typedstream/MultiPartWithDeleted");
        let mut file = File::open(typedstream_path).unwrap();
        let mut bytes = vec![];
        file.read_to_end(&mut bytes).unwrap();

        let mut parser = TypedStreamReader::from(&bytes);
        message.components = parser.parse().ok();

        let expected = r#"{"rowid":0,"guid":"","text":"From arbitrary byte stream:\r￼To native Rust data structures:\r","service":"iMessage","handle_id":0,"destination_caller_id":null,"subject":null,"date":674526582885055488,"date_read":0,"date_delivered":0,"is_from_me":true,"is_read":false,"item_type":0,"other_handle":0,"share_status":false,"share_direction":false,"group_title":null,"group_action_type":0,"associated_message_guid":null,"associated_message_type":0,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":0,"associated_message_emoji":null,"chat_id":0,"num_attachments":0,"deleted_from":null,"num_replies":0,"components":[{"Object":[{"name":"NSMutableString","version":1},[{"String":"From arbitrary byte stream:\r￼To native Rust data structures:\r"}]]},{"Data":[{"SignedInteger":1},{"UnsignedInteger":28}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":1}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":0}]]},{"Data":[{"SignedInteger":2},{"UnsignedInteger":1}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":2}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMFileTransferGUIDAttributeName"}]]},{"Object":[{"name":"NSString","version":1},[{"String":"D0551D89-4E11-43D0-9A0E-06F19704E97B"}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":1}]]},{"Data":[{"SignedInteger":3},{"UnsignedInteger":32}]},{"Object":[{"name":"NSDictionary","version":0},[{"SignedInteger":1}]]},{"Object":[{"name":"NSString","version":1},[{"String":"__kIMMessagePartAttributeName"}]]},{"Object":[{"name":"NSNumber","version":0},[{"SignedInteger":2}]]}],"edited_parts":null}"#;
        let actual = exporter.format_message(&message, 0).unwrap();

        assert_eq!(expected, actual);
    }

    #[test]
    fn can_format_json_conversion_fully_unsent() {
        // Set timezone to America/Los_Angeles for consistent Local time
        set_var("TZ", "America/Los_Angeles");

        // Create exporter
        let options = fake_options();
        let config = fake_config(options);
        let exporter = JSON::new(&config).unwrap();

        let mut message = blank();
        // May 17, 2022  8:29:42 PM
        message.date = 674526582885055488;
        message.date_edited = 674530231992568192;
        message.text = None;
        message.is_from_me = true;
        message.chat_id = Some(0);
        message.edited_parts = Some(EditedMessage {
            parts: vec![EditedMessagePart {
                status: EditStatus::Unsent,
                edit_history: vec![],
            }],
        });

        let typedstream_path = current_dir()
            .unwrap()
            .parent()
            .unwrap()
            .join("imessage-database/test_data/typedstream/Blank");
        let mut file = File::open(typedstream_path).unwrap();
        let mut bytes = vec![];
        file.read_to_end(&mut bytes).unwrap();

        let mut parser = TypedStreamReader::from(&bytes);
        message.components = parser.parse().ok();

        let expected = r#"{"rowid":0,"guid":"","text":null,"service":"iMessage","handle_id":0,"destination_caller_id":null,"subject":null,"date":674526582885055488,"date_read":0,"date_delivered":0,"is_from_me":true,"is_read":false,"item_type":0,"other_handle":0,"share_status":false,"share_direction":false,"group_title":null,"group_action_type":0,"associated_message_guid":null,"associated_message_type":0,"balloon_bundle_id":null,"expressive_send_style_id":null,"thread_originator_guid":null,"thread_originator_part":null,"date_edited":674530231992568192,"associated_message_emoji":null,"chat_id":0,"num_attachments":0,"deleted_from":null,"num_replies":0,"components":[{"Object":[{"name":"NSString","version":1},[{"String":""}]]}],"edited_parts":{"parts":[{"status":"Unsent","edit_history":[]}]}}"#;
        let actual = exporter.format_announcement(&message);

        assert_eq!(expected, actual);
    }
}
