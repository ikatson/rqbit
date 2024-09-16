pub const UPNP_DEVICE_ROOT: &str = "upnp:rootdevice";
pub const UPNP_DEVICE_MEDIASERVER: &str = "urn:schemas-upnp-org:device:MediaServer:1";

pub const SOAP_ACTION_CONTENT_DIRECTORY_BROWSE: &[u8] =
    b"\"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"";
pub const SOAP_ACTION_GET_SYSTEM_UPDATE_ID: &[u8] =
    b"\"urn:schemas-upnp-org:service:ContentDirectory:1#GetSystemUpdateID\"";

pub const CONTENT_TYPE_XML_UTF8: &str = "text/xml; charset=\"utf-8\"";
