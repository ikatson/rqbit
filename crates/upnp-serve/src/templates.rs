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
                tmpl.replace("{id}", &item.id.to_string())
                    .replace("{parent_id}", &item.parent_id.unwrap_or(0).to_string())
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
                .replace("{parent_id}", &item.parent_id.unwrap_or(0).to_string())
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

    use std::time::{SystemTime, UNIX_EPOCH};
    let update_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    render_content_directory_envelope(&Envelope {
        result: &result,
        number_returned: total,
        total_matches: total,
        update_id,
    })
}

pub fn render_notify_subscription_system_update_id(update_id: u64) -> String {
    include_str!("resources/templates/notify_subscription.tmpl.xml")
        .replace("{system_update_id}", &update_id.to_string())
}

pub fn render_content_directory_control_get_system_update_id(update_id: u64) -> String {
    include_str!("resources/templates/content_directory_control_get_system_update_id.tmpl.xml")
        .replace("{id}", &update_id.to_string())
}
