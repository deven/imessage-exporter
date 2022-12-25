/*!
 Errors that can happen when extracting data from a SQLite table
*/

use std::fmt::{Display, Formatter, Result};

use rusqlite::Error;

#[derive(Debug)]
pub enum TableError {
    Attachment(Error),
    ChatToHandle(Error),
    Chat(Error),
    Handle(Error),
    Messages(Error),
}

impl Display for TableError {
    fn fmt(&self, fmt: &mut Formatter<'_>) -> Result {
        match self {
            TableError::Attachment(why) => write!(fmt, "Failed to parse row: {why}"),
            TableError::ChatToHandle(why) => write!(fmt, "Failed to parse row: {why}"),
            TableError::Chat(why) => write!(fmt, "Failed to parse row: {why}"),
            TableError::Handle(why) => write!(fmt, "Failed to parse row: {why}"),
            TableError::Messages(why) => write!(fmt, "Failed to parse row: {why}"),
        }
    }
}
