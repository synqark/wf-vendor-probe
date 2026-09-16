//! Built-in needle sets.
//!
//! A needle is a literal byte string that appears inside the JSON we are trying
//! to find. Good needles are key names or path prefixes that the API response
//! always contains and that surrounding heap data never does.

pub struct Preset {
    pub name: &'static str,
    pub about: &'static str,
    pub needles: &'static [&'static str],
}

/// Vendor stock manifests — the `getVendorInfo.php` response shape.
///
/// These are the keys DE uses for rotating vendor inventories (Varzia,
/// Archimedean Yonta, Acrithis, Palladino, the syndicate offerings, and so on).
/// `ItemManifest` and `VendorManifests` are the two strongest signals; the rest
/// are per-offer fields that help when a response is fragmented.
const VENDOR: &[&str] = &[
    "\"ItemManifest\"",
    "/Lotus/Types/Game/VendorManifests/",
    "\"PurchaseQuantityLimit\"",
    "\"AllowMultipurchase\"",
    "\"RandomSeedType\"",
    "\"PropertyTextHash\"",
];

/// Everything in `vendor`, plus fields that also occur elsewhere.
///
/// Use this when the tight set finds nothing — it trades precision for reach.
const VENDOR_WIDE: &[&str] = &[
    "\"ItemManifest\"",
    "/Lotus/Types/Game/VendorManifests/",
    "\"PurchaseQuantityLimit\"",
    "\"AllowMultipurchase\"",
    "\"RandomSeedType\"",
    "\"PropertyTextHash\"",
    "\"StoreItem\"",
    "\"ItemPrices\"",
    "\"QuantityMultiplier\"",
    "\"RotatedWeekly\"",
    "\"PrimeVaultTraderInfo\"",
    "\"PurchaseAvailability\"",
    "/Lotus/StoreItems/",
];

/// Endpoint discovery: what URLs the client actually calls.
///
/// Run this first if the vendor presets come up empty — the request URL is
/// usually still in the heap next to the response, and it names the endpoint.
const API: &[&str] = &[
    "getVendorInfo",
    "VendorManifest",
    "api.warframe.com",
    "/api/",
    ".php?",
    "&accountId=",
];

/// The full-account inventory blob, as a pipeline self-test.
///
/// If this preset finds nothing while the game sits in the orbiter, the problem
/// is process access, not the vendor needles.
const INVENTORY: &[&str] = &[
    "\"SubscribedToEmails\"",
    "\"MiscItems\":[",
    "\"Suits\":[",
];

pub const PRESETS: &[Preset] = &[
    Preset {
        name: "vendor",
        about: "vendor stock manifests (tight, few false positives) [default]",
        needles: VENDOR,
    },
    Preset {
        name: "vendor-wide",
        about: "vendor manifests plus per-offer fields (more reach, more noise)",
        needles: VENDOR_WIDE,
    },
    Preset {
        name: "api",
        about: "request URLs and endpoint names, for discovery",
        needles: API,
    },
    Preset {
        name: "inventory",
        about: "the full-account inventory blob, as a self-test",
        needles: INVENTORY,
    },
];

pub fn lookup(name: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.name == name)
}
