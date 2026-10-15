mod records;
mod inline {
    pub struct Inline {
        pub note: Option<String>,
    }
}
#[path = "relocated.rs"]
mod relocated;
macro_rules! events {
    () => {
        struct Envelope {
            subtype: Option<String>,
            #[serde(rename = "privateWire")]
            metadata: Option<Option<String>>,
        }
    }
}
events! {
    Example("example", "", "") {
        records: Vec<Box<records::Record>>,
        inline: inline::Inline,
        relocated: relocated::Relocated,
    } optional {
        optional_records: Option<Vec<Box<records::Record>>>,
    }
}
