use crate::upnp_types::content_directory::response::{Container, Item, ItemOrContainer};

pub struct RootDescriptionInputs<'a> {
    pub friendly_name: &'a str,
    pub manufacturer: &'a str,
    pub model_name: &'a str,
    pub unique_id: &'a str,
    pub http_prefix: &'a str,
}

pub fn render_root_description_xml(input: &RootDescriptionInputs<'_>) -> String {
    format!(
        include_str!("resources/templates/root_desc.tmpl.xml"),
        friendly_name = input.friendly_name,
        manufacturer = input.manufacturer,
        model_name = input.model_name,
        unique_id = input.unique_id,
        http_prefix = input.http_prefix
    )
}

pub fn render_content_directory_browse(items: impl IntoIterator<Item = ItemOrContainer>) -> String {
    fn item_or_container(item_or_container: &ItemOrContainer) -> Option<String> {
        fn item(item: &Item) -> Option<String> {
            let mime = item.mime_type.as_ref()?;
            let upnp_class = match mime.type_().as_str() {
                "video" => "object.item.videoItem",
                _ => return None,
            };
            let mime = mime.to_string();

            Some(format!(
                include_str!("resources/templates/content_directory/control/browse/item.tmpl.xml"),
                id = item.id,
                parent_id = item.parent_id.unwrap_or(0),
                mime_type = mime,
                url = item.url,
                upnp_class = upnp_class,
                title = item.title
            ))
        }

        fn container(item: &Container) -> String {
            let child_count_tag = match item.children_count {
                Some(cc) => format!("childCount=\"{}\"", cc),
                None => String::new(),
            };
            format!(
                include_str!(
                    "resources/templates/content_directory/control/browse/container.tmpl.xml"
                ),
                id = item.id,
                parent_id = item.parent_id.unwrap_or(0),
                title = item.title,
                childCountTag = child_count_tag
            )
        }

        match item_or_container {
            ItemOrContainer::Container(c) => Some(container(c)),
            ItemOrContainer::Item(i) => item(i),
        }
    }

    struct Envelope<'a> {
        items: &'a str,
        number_returned: usize,
        total_matches: usize,
        update_id: u64,
    }

    fn render_response(envelope: &Envelope<'_>) -> String {
        format!(
            include_str!("resources/templates/content_directory/control/browse/response.tmpl.xml"),
            items = envelope.items,
            number_returned = envelope.number_returned,
            total_matches = envelope.total_matches,
            update_id = envelope.update_id
        )
    }

    let all_items = items
        .into_iter()
        .filter_map(|item| item_or_container(&item))
        .collect::<Vec<_>>();
    let total = all_items.len();
    let all_items = all_items.join("");

    use std::time::{SystemTime, UNIX_EPOCH};
    let update_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    render_response(&Envelope {
        items: &all_items,
        number_returned: total,
        total_matches: total,
        update_id,
    })
}

pub fn render_notify_subscription_system_update_id(update_id: u64) -> String {
    format!(
        include_str!(
            "resources/templates/content_directory/subscriptions/system_update_id.tmpl.xml"
        ),
        system_update_id = update_id
    )
}

pub fn render_content_directory_control_get_system_update_id(update_id: u64) -> String {
    format!(
        include_str!(
            "resources/templates/content_directory/control/get_system_update_id/response.tmpl.xml"
        ),
        id = update_id
    )
}
