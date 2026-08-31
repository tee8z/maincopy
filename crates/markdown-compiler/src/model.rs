use crate::{LogicalContentPath, PostCollection};

#[derive(Clone, Debug)]
pub struct PublicationSource<'source> {
    pub path: LogicalContentPath,
    pub contents: &'source str,
}

impl<'source> PublicationSource<'source> {
    pub fn new(path: impl Into<String>, contents: &'source str) -> Self {
        Self {
            path: LogicalContentPath::new(path),
            contents,
        }
    }
}

#[derive(Clone, Debug)]
pub struct PostSource<'source> {
    pub path: LogicalContentPath,
    pub contents: &'source str,
    pub collection: PostCollection,
}

impl<'source> PostSource<'source> {
    pub fn in_posts(path: impl Into<String>, contents: &'source str) -> Self {
        Self {
            path: LogicalContentPath::new(path),
            contents,
            collection: PostCollection::Posts,
        }
    }

    pub fn in_drafts(path: impl Into<String>, contents: &'source str) -> Self {
        Self {
            path: LogicalContentPath::new(path),
            contents,
            collection: PostCollection::Drafts,
        }
    }
}
