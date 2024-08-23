use std::sync::atomic::{AtomicU64, Ordering};

use crate::upnp_types::content_directory::response::{Container, Item, ItemOrContainer};

pub struct RootDescriptionInputs<'a> {
    pub friendly_name: &'a str,
    pub manufacturer: &'a str,
    pub model_name: &'a str,
    pub unique_id: &'a str,
    pub http_prefix: &'a str,
}

pub fn render_root_description_xml(input: &RootDescriptionInputs<'_>) -> String {
    let tmpl = include_str!("resources/templates/root_desc.tmpl.xml").trim();

    // This isn't great perf-wise but whatever.
    tmpl.replace("{friendly_name}", input.friendly_name)
        .replace("{manufacturer}", input.manufacturer)
        .replace("{model_name}", input.model_name)
        .replace("{unique_id}", input.unique_id)
        .replace("{http_prefix}", input.http_prefix)
}

pub fn render_content_directory_browse(items: impl IntoIterator<Item = ItemOrContainer>) -> String {
    fn item_or_container(item_or_container: &ItemOrContainer) -> Option<String> {
        fn item(item: &Item) -> Option<String> {
            let tmpl =
                include_str!("resources/templates/content_directory_control_browse_item.tmpl.xml")
                    .trim();

            let mime = item.mime_type.as_ref()?;
            let upnp_class = match mime.type_().as_str() {
                "video" => "object.item.videoItem",
                _ => return None,
            };
            let mime = mime.to_string();

            Some(
                tmpl.replace("{id}", &format!("{}", item.id))
                    .replace("{parent_id}", &format!("{}", item.parent_id.unwrap_or(0)))
                    .replace("{mime_type}", &mime)
                    .replace("{url}", &item.url)
                    .replace("{upnp_class}", upnp_class)
                    .replace("{title}", &item.title),
            )
        }

        fn container(item: &Container) -> String {
            let tmpl = include_str!(
                "resources/templates/content_directory_control_browse_container.tmpl.xml"
            )
            .trim();
            tmpl.replace("{id}", &format!("{}", item.id))
                .replace("{parent_id}", &format!("{}", item.parent_id.unwrap_or(0)))
                .replace("{title}", &item.title)
                .replace(
                    "{childCountTag}",
                    &match item.children_count {
                        Some(cc) => format!("childCount=\"{}\"", cc),
                        None => String::new(),
                    },
                )
        }

        match item_or_container {
            ItemOrContainer::Container(c) => Some(container(c)),
            ItemOrContainer::Item(i) => item(i),
        }
    }

    struct Envelope<'a> {
        result: &'a str,
        number_returned: usize,
        total_matches: usize,
        update_id: u64,
    }

    fn render_content_directory_envelope(envelope: &Envelope<'_>) -> String {
        let tmpl =
            include_str!("resources/templates/content_directory_control_browse_envelope.tmpl.xml")
                .trim();
        tmpl.replace("{result}", envelope.result)
            .replace("{number_returned}", &envelope.number_returned.to_string())
            .replace("{total_matches}", &envelope.total_matches.to_string())
            .replace("{update_id}", &envelope.update_id.to_string())
    }

    fn render_content_directory_browse_result(items: &str) -> String {
        let tmpl =
            include_str!("resources/templates/content_directory_control_browse_result.tmpl.xml")
                .trim();
        tmpl.replace("{items}", items)
    }

    let all_items = items
        .into_iter()
        .filter_map(|item| item_or_container(&item))
        .collect::<Vec<_>>();
    let total = all_items.len();
    let all_items = all_items.join("");

    let result = render_content_directory_browse_result(&all_items);

    // TODO: use smth better
    static UPDATE_ID: AtomicU64 = AtomicU64::new(1);
    let update_id = UPDATE_ID.fetch_add(1, Ordering::Relaxed);

    render_content_directory_envelope(&Envelope {
        result: &result,
        number_returned: total,
        total_matches: total,
        update_id,
    })
}
