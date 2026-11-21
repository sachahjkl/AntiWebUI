//! The little of SPA's pods this reads and writes: the `Props` of a node (its volumes, whether it
//! is muted, where its channels go), and the profiles and routes (ports) of a device of cards,
//! whose route in use holds the sound of a node of it. A pod is a size and a type, then a body
//! padded to 8 bytes; values are in the machine's byte order.

pub const TYPE_BOOL: u32 = 2;
pub const TYPE_ID: u32 = 3;
pub const TYPE_INT: u32 = 4;
pub const TYPE_FLOAT: u32 = 6;
pub const TYPE_STRING: u32 = 8;
pub const TYPE_ARRAY: u32 = 13;
pub const TYPE_OBJECT: u32 = 15;
pub const TYPE_CHOICE: u32 = 19;
/// `SPA_TYPE_OBJECT_Props`.
pub const OBJECT_PROPS: u32 = 0x40002;
/// `SPA_PARAM_Props`.
pub const PARAM_PROPS: u32 = 2;
pub const PROP_VOLUME: u32 = 0x10003;
pub const PROP_MUTE: u32 = 0x10004;
pub const PROP_CHANNEL_VOLUMES: u32 = 0x10008;
pub const PROP_CHANNEL_MAP: u32 = 0x1000b;
/// `SPA_TYPE_OBJECT_ParamProfile`.
pub const OBJECT_PROFILE: u32 = 0x40007;
/// `SPA_PARAM_EnumProfile` and `SPA_PARAM_Profile`.
pub const PARAM_ENUM_PROFILE: u32 = 8;
pub const PARAM_PROFILE: u32 = 9;
/// The keys of a `ParamProfile` (`SPA_PARAM_PROFILE_*`).
pub const PROFILE_INDEX: u32 = 1;
pub const PROFILE_NAME: u32 = 2;
pub const PROFILE_DESCRIPTION: u32 = 3;
pub const PROFILE_PRIORITY: u32 = 4;
pub const PROFILE_AVAILABLE: u32 = 5;
pub const PROFILE_SAVE: u32 = 8;
/// `SPA_TYPE_OBJECT_ParamRoute`.
pub const OBJECT_ROUTE: u32 = 0x40009;
/// `SPA_PARAM_EnumRoute` and `SPA_PARAM_Route`.
pub const PARAM_ENUM_ROUTE: u32 = 12;
pub const PARAM_ROUTE: u32 = 13;
/// The keys of a `ParamRoute` (`SPA_PARAM_ROUTE_*`).
pub const ROUTE_INDEX: u32 = 1;
pub const ROUTE_DIRECTION: u32 = 2;
pub const ROUTE_DEVICE: u32 = 3;
pub const ROUTE_NAME: u32 = 4;
pub const ROUTE_DESCRIPTION: u32 = 5;
pub const ROUTE_PRIORITY: u32 = 6;
pub const ROUTE_AVAILABLE: u32 = 7;
pub const ROUTE_PROFILES: u32 = 9;
pub const ROUTE_PROPS: u32 = 10;
pub const ROUTE_DEVICES: u32 = 11;
pub const ROUTE_SAVE: u32 = 13;
/// `SPA_PARAM_AVAILABILITY_no` and `_yes`; `_unknown` is 0.
pub const AVAILABLE_NO: u32 = 1;
pub const AVAILABLE_YES: u32 = 2;
/// `SPA_DIRECTION_OUTPUT`: a route sound goes out through; an input is 0.
pub const DIRECTION_OUTPUT: u32 = 1;

/// What a `Props` pod says of the sound.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Props {
    /// Linear, one for each channel.
    pub volumes: Option<Vec<f32>>,
    /// Linear, for all channels, when there is no array.
    pub volume: Option<f32>,
    pub mute: Option<bool>,
    /// Where each channel goes, as `SPA_AUDIO_CHANNEL_*`.
    pub channel_map: Option<Vec<u32>>,
}

/// A profile of a card, from an `EnumProfile` or a `Profile` pod.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Profile {
    pub index: u32,
    pub name: String,
    pub description: String,
    pub priority: i32,
    /// `SPA_PARAM_AVAILABILITY_*`.
    pub available: u32,
}

/// A route of a card (a port, like headphones), from an `EnumRoute` pod, or from a `Route` pod:
/// the way out or in a device of a card uses now, and its sound.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Route {
    pub index: i32,
    /// The device of the card it is for, which a node of it gives as `card.profile.device`; -1
    /// in an `EnumRoute` pod, which gives `devices`.
    pub device: i32,
    pub props: Props,
    /// `SPA_DIRECTION_*`.
    pub direction: u32,
    pub name: String,
    pub description: String,
    pub priority: i32,
    /// `SPA_PARAM_AVAILABILITY_*`.
    pub available: u32,
    /// The profiles it is there in, by index; empty when the pod does not say.
    pub profiles: Vec<i32>,
    /// The card's devices it can be a route of, in an `EnumRoute` pod.
    pub devices: Vec<i32>,
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn pad8(n: usize) -> usize {
    (n + 7) & !7
}

/// The properties of an object of a kind, given the whole pod with its header: each one's key
/// and its value, a pod with its header.
fn properties(pod: &[u8], object: u32) -> Option<Vec<(u32, &[u8])>> {
    let size = u32_at(pod, 0)? as usize;
    if u32_at(pod, 4)? != TYPE_OBJECT || pod.len() < 8 + size || size < 8 {
        return None;
    }
    if u32_at(pod, 8)? != object {
        return None;
    }
    let end = 8 + size;
    let mut at = 16;
    let mut out = Vec::new();
    // Each property: a key, flags, then a pod.
    while at + 16 <= end {
        let key = u32_at(pod, at)?;
        let value_size = u32_at(pod, at + 8)? as usize;
        out.push((key, pod.get(at + 8..at + 16 + value_size)?));
        at += 16 + pad8(value_size);
    }
    Some(out)
}

/// The type and the body of a pod with its header.
fn split(value: &[u8]) -> (u32, &[u8]) {
    (u32_at(value, 4).unwrap_or(0), &value[8..])
}

/// Reads a `Props` object, given the whole pod with its header.
pub fn parse_props(pod: &[u8]) -> Option<Props> {
    let mut props = Props::default();
    for (key, value) in properties(pod, OBJECT_PROPS)? {
        let (value_type, body) = split(value);
        match key {
            PROP_VOLUME => props.volume = float(value_type, body),
            PROP_MUTE => props.mute = boolean(value_type, body),
            PROP_CHANNEL_VOLUMES if value_type == TYPE_ARRAY => props.volumes = floats(body),
            PROP_CHANNEL_MAP if value_type == TYPE_ARRAY => {
                props.channel_map = array(body, TYPE_ID);
            }
            _ => {}
        }
    }
    Some(props)
}

/// Reads a `ParamProfile` object, given the whole pod with its header.
pub fn parse_profile(pod: &[u8]) -> Option<Profile> {
    let mut index = None;
    let mut profile = Profile::default();
    for (key, value) in properties(pod, OBJECT_PROFILE)? {
        let (t, body) = split(value);
        match key {
            PROFILE_INDEX => index = int(t, body),
            PROFILE_NAME => profile.name = string(t, body).unwrap_or_default(),
            PROFILE_DESCRIPTION => profile.description = string(t, body).unwrap_or_default(),
            PROFILE_PRIORITY => profile.priority = int(t, body).unwrap_or(0),
            PROFILE_AVAILABLE => profile.available = id(t, body).unwrap_or(0),
            _ => {}
        }
    }
    profile.index = index? as u32;
    Some(profile)
}

/// Reads a `Route` or an `EnumRoute` object, given the whole pod with its header.
pub fn parse_route(pod: &[u8]) -> Option<Route> {
    let mut index = None;
    let mut route = Route {
        device: -1,
        ..Route::default()
    };
    for (key, value) in properties(pod, OBJECT_ROUTE)? {
        let (t, body) = split(value);
        match key {
            ROUTE_INDEX => index = int(t, body),
            ROUTE_DEVICE => route.device = int(t, body).unwrap_or(-1),
            ROUTE_PROPS => route.props = parse_props(value)?,
            ROUTE_DIRECTION => route.direction = id(t, body).unwrap_or(0),
            ROUTE_NAME => route.name = string(t, body).unwrap_or_default(),
            ROUTE_DESCRIPTION => route.description = string(t, body).unwrap_or_default(),
            ROUTE_PRIORITY => route.priority = int(t, body).unwrap_or(0),
            ROUTE_AVAILABLE => route.available = id(t, body).unwrap_or(0),
            ROUTE_PROFILES if t == TYPE_ARRAY => route.profiles = ints(body),
            ROUTE_DEVICES if t == TYPE_ARRAY => route.devices = ints(body),
            _ => {}
        }
    }
    route.index = index?;
    Some(route)
}

/// The value of a pod of a type, or the current one of a choice of them.
fn unwrap_choice(kind: u32, body: &[u8]) -> Option<(u32, &[u8])> {
    if kind != TYPE_CHOICE {
        return Some((kind, body));
    }
    // The kind of choice and flags, then the child pod's header, then its values.
    let child_size = u32_at(body, 8)? as usize;
    let child_type = u32_at(body, 12)?;
    Some((child_type, body.get(16..16 + child_size)?))
}

fn float(kind: u32, body: &[u8]) -> Option<f32> {
    let (kind, body) = unwrap_choice(kind, body)?;
    (kind == TYPE_FLOAT).then(|| Some(f32::from_ne_bytes(body.get(..4)?.try_into().ok()?)))?
}

fn int(kind: u32, body: &[u8]) -> Option<i32> {
    let (kind, body) = unwrap_choice(kind, body)?;
    (kind == TYPE_INT).then(|| Some(u32_at(body, 0)? as i32))?
}

fn id(kind: u32, body: &[u8]) -> Option<u32> {
    let (kind, body) = unwrap_choice(kind, body)?;
    (kind == TYPE_ID).then(|| u32_at(body, 0))?
}

fn boolean(kind: u32, body: &[u8]) -> Option<bool> {
    let (kind, body) = unwrap_choice(kind, body)?;
    (kind == TYPE_BOOL).then(|| Some(u32_at(body, 0)? != 0))?
}

/// A string pod's text, without its terminating null.
fn string(kind: u32, body: &[u8]) -> Option<String> {
    if kind != TYPE_STRING {
        return None;
    }
    let end = body.iter().position(|&b| b == 0).unwrap_or(body.len());
    Some(String::from_utf8_lossy(&body[..end]).into_owned())
}

/// The 4-byte values of an array whose children are of `child_type`.
fn array(body: &[u8], child_type: u32) -> Option<Vec<u32>> {
    // The child's size and type, then the values one after another.
    let child_size = u32_at(body, 0)? as usize;
    if u32_at(body, 4)? != child_type || child_size != 4 {
        return None;
    }
    Some(
        body[8..]
            .chunks_exact(4)
            .map(|c| u32::from_ne_bytes(c.try_into().expect("four bytes")))
            .collect(),
    )
}

fn floats(body: &[u8]) -> Option<Vec<f32>> {
    Some(
        array(body, TYPE_FLOAT)?
            .into_iter()
            .map(f32::from_bits)
            .collect(),
    )
}

fn ints(body: &[u8]) -> Vec<i32> {
    array(body, TYPE_INT)
        .unwrap_or_default()
        .into_iter()
        .map(|v| v as i32)
        .collect()
}

/// A `Props` object setting what is given, in words so that it is aligned for SPA.
pub fn build_props(volumes: Option<&[f32]>, mute: Option<bool>) -> Vec<u64> {
    words(&props_pod(volumes, mute))
}

/// A `Route` object setting what is given of the sound of a route, kept for the next time it is
/// used, in words so that it is aligned for SPA.
pub fn build_route(route: &Route, volumes: Option<&[f32]>, mute: Option<bool>) -> Vec<u64> {
    words(&route_pod(
        route.index,
        route.device,
        Some(props_pod(volumes, mute)),
    ))
}

/// A `Route` object choosing the route `index` for the card's device `device`, kept for the next
/// time, in words so that it is aligned for SPA.
pub fn build_port(index: i32, device: i32) -> Vec<u64> {
    words(&route_pod(index, device, None))
}

/// A `Profile` object choosing the card's profile `index`, kept for the next time, in words so
/// that it is aligned for SPA.
pub fn build_profile(index: u32) -> Vec<u64> {
    let mut body: Vec<u8> = Vec::new();
    push(&mut body, OBJECT_PROFILE);
    push(&mut body, PARAM_PROFILE);
    for v in [PROFILE_INDEX, 0, 4, TYPE_INT, index, 0] {
        push(&mut body, v);
    }
    for v in [PROFILE_SAVE, 0, 4, TYPE_BOOL, 1, 0] {
        push(&mut body, v);
    }
    words(&object(body))
}

/// A `Route` pod with its header: the route `index` of the device `device`, with the `Props`
/// pod given, saved.
fn route_pod(index: i32, device: i32, props: Option<Vec<u8>>) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::new();
    push(&mut body, OBJECT_ROUTE);
    push(&mut body, PARAM_ROUTE);
    for (key, value) in [(ROUTE_INDEX, index), (ROUTE_DEVICE, device)] {
        for v in [key, 0, 4, TYPE_INT, value as u32, 0] {
            push(&mut body, v);
        }
    }
    if let Some(props) = props {
        push(&mut body, ROUTE_PROPS);
        push(&mut body, 0);
        body.extend_from_slice(&props);
    }
    for v in [ROUTE_SAVE, 0, 4, TYPE_BOOL, 1, 0] {
        push(&mut body, v);
    }
    object(body)
}

fn push(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_ne_bytes());
}

/// An object pod with its header, around a body of its kind, id and properties.
fn object(body: Vec<u8>) -> Vec<u8> {
    let mut pod = Vec::with_capacity(8 + body.len());
    push(&mut pod, body.len() as u32);
    push(&mut pod, TYPE_OBJECT);
    pod.extend_from_slice(&body);
    pod
}

/// A `Props` pod with its header, a whole number of words long.
fn props_pod(volumes: Option<&[f32]>, mute: Option<bool>) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::new();
    push(&mut body, OBJECT_PROPS);
    push(&mut body, PARAM_PROPS);
    if let Some(volumes) = volumes {
        push(&mut body, PROP_CHANNEL_VOLUMES);
        push(&mut body, 0);
        push(&mut body, 8 + 4 * volumes.len() as u32);
        push(&mut body, TYPE_ARRAY);
        push(&mut body, 4);
        push(&mut body, TYPE_FLOAT);
        for v in volumes {
            body.extend_from_slice(&v.to_ne_bytes());
        }
        body.resize(pad8(body.len()), 0);
    }
    if let Some(mute) = mute {
        for v in [PROP_MUTE, 0, 4, TYPE_BOOL, mute as u32, 0] {
            push(&mut body, v);
        }
    }
    object(body)
}

fn words(pod: &[u8]) -> Vec<u64> {
    pod.chunks(8)
        .map(|c| {
            let mut w = [0u8; 8];
            w[..c.len()].copy_from_slice(c);
            u64::from_ne_bytes(w)
        })
        .collect()
}

/// The short name of a channel position (`SPA_AUDIO_CHANNEL_*`), like `FL`.
pub fn channel_name(position: u32) -> &'static str {
    const NAMES: [&str; 38] = [
        "UNK", "NA", "MONO", "FL", "FR", "FC", "LFE", "SL", "SR", "FLC", "FRC", "RC", "RL", "RR",
        "TC", "TFL", "TFC", "TFR", "TRL", "TRC", "TRR", "RLC", "RRC", "FLW", "FRW", "LFE2", "FLH",
        "FCH", "FRH", "TFLC", "TFRC", "TSL", "TSR", "LLFE", "RLFE", "BC", "BLC", "BRC",
    ];
    NAMES.get(position as usize).copied().unwrap_or("AUX")
}

/// The bytes of words.
#[cfg(test)]
pub fn bytes(words: &[u64]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_ne_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn props_go_both_ways() {
        let pod = bytes(&build_props(Some(&[0.125, 0.5]), Some(true)));
        assert_eq!(pod.len() % 8, 0);
        let props = parse_props(&pod).unwrap();
        assert_eq!(props.volumes, Some(vec![0.125, 0.5]));
        assert_eq!(props.mute, Some(true));
        assert_eq!(props.volume, None);
        let only_mute = parse_props(&bytes(&build_props(None, Some(false)))).unwrap();
        assert_eq!((only_mute.volumes, only_mute.mute), (None, Some(false)));
    }

    #[test]
    fn what_pipewire_sends_is_read() {
        // As a node sends it: a float volume, a bool mute in a choice-less form, a three-channel
        // array, a channel map, and a property this doesn't know.
        let mut b: Vec<u8> = Vec::new();
        let w = |b: &mut Vec<u8>, v: u32| b.extend_from_slice(&v.to_ne_bytes());
        w(&mut b, OBJECT_PROPS);
        w(&mut b, PARAM_PROPS);
        for (key, kind, value) in [
            (PROP_VOLUME, TYPE_FLOAT, 1.0f32.to_bits()),
            (PROP_MUTE, TYPE_BOOL, 0),
            (0x10005, 4, 7),
        ] {
            w(&mut b, key);
            w(&mut b, 0);
            w(&mut b, 4);
            w(&mut b, kind);
            w(&mut b, value);
            w(&mut b, 0);
        }
        w(&mut b, PROP_CHANNEL_VOLUMES);
        w(&mut b, 0);
        w(&mut b, 8 + 12);
        w(&mut b, TYPE_ARRAY);
        w(&mut b, 4);
        w(&mut b, TYPE_FLOAT);
        for v in [0.25f32, 0.5, 1.0] {
            w(&mut b, v.to_bits());
        }
        w(&mut b, 0);
        w(&mut b, PROP_CHANNEL_MAP);
        w(&mut b, 0);
        w(&mut b, 8 + 8);
        w(&mut b, TYPE_ARRAY);
        w(&mut b, 4);
        w(&mut b, TYPE_ID);
        w(&mut b, 3);
        w(&mut b, 4);
        let mut pod = Vec::new();
        w(&mut pod, b.len() as u32);
        w(&mut pod, TYPE_OBJECT);
        pod.extend_from_slice(&b);
        let props = parse_props(&pod).unwrap();
        assert_eq!(props.volume, Some(1.0));
        assert_eq!(props.mute, Some(false));
        assert_eq!(props.volumes, Some(vec![0.25, 0.5, 1.0]));
        assert_eq!(props.channel_map, Some(vec![3, 4]));
        // Not an object of props.
        assert_eq!(parse_props(&pod[..12]), None);
    }

    #[test]
    fn routes_go_both_ways() {
        let route = Route {
            index: 4,
            device: 1,
            ..Route::default()
        };
        let pod = bytes(&build_route(&route, Some(&[0.0, 0.25]), Some(true)));
        assert_eq!(pod.len() % 8, 0);
        let read = parse_route(&pod).unwrap();
        assert_eq!((read.index, read.device), (4, 1));
        assert_eq!(read.props.volumes, Some(vec![0.0, 0.25]));
        assert_eq!(read.props.mute, Some(true));
        // A route is not props, nor props a route.
        assert_eq!(parse_props(&pod), None);
        assert_eq!(parse_route(&bytes(&build_props(None, Some(true)))), None);
    }

    #[test]
    fn profiles_and_ports_are_read() {
        let pod = bytes(&build_profile(3));
        let profile = parse_profile(&pod).unwrap();
        assert_eq!(profile.index, 3);
        let pod = bytes(&build_port(5, 1));
        let route = parse_route(&pod).unwrap();
        assert_eq!((route.index, route.device), (5, 1));
        assert_eq!(route.props, Props::default());
        // Strings, as a card sends them.
        let mut b: Vec<u8> = Vec::new();
        let w = |b: &mut Vec<u8>, v: u32| b.extend_from_slice(&v.to_ne_bytes());
        w(&mut b, OBJECT_PROFILE);
        w(&mut b, PARAM_ENUM_PROFILE);
        w(&mut b, PROFILE_INDEX);
        w(&mut b, 0);
        w(&mut b, 4);
        w(&mut b, TYPE_INT);
        w(&mut b, 2);
        w(&mut b, 0);
        w(&mut b, PROFILE_DESCRIPTION);
        w(&mut b, 0);
        w(&mut b, 4);
        w(&mut b, TYPE_STRING);
        b.extend_from_slice(b"Off\0\0\0\0\0");
        let mut pod = Vec::new();
        w(&mut pod, b.len() as u32);
        w(&mut pod, TYPE_OBJECT);
        pod.extend_from_slice(&b);
        let profile = parse_profile(&pod).unwrap();
        assert_eq!((profile.index, profile.description.as_str()), (2, "Off"));
    }

    #[test]
    fn channels_are_named() {
        assert_eq!(channel_name(3), "FL");
        assert_eq!(channel_name(4), "FR");
        assert_eq!(channel_name(500), "AUX");
    }
}
