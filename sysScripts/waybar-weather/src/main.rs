//! Waybar Weather Module
//!
//! Fetches GeoClue coordinates, resolves the location name, and emits a
//! compact Waybar JSON payload. The last successful payload is cached for the
//! lockscreen and used as a fallback when live data fails.

use futures_util::StreamExt;
use std::env;
use std::fs;
use std::path::PathBuf;
use tokio::time::{Duration, timeout};
use zbus::zvariant::OwnedObjectPath;
use zbus::{Connection, Proxy};

const WAYBAR_CACHE_FILE: &str = ".weather_cache.json";
const LOCKSCREEN_CACHE_FILE: &str = ".weather_cache";

#[derive(Debug, Clone, Copy)]
struct Location {
    lat: f64,
    lon: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    Celsius,
    Fahrenheit,
}

impl Unit {
    fn from_arg(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "c" | "celsius" => Some(Self::Celsius),
            "f" | "fahrenheit" => Some(Self::Fahrenheit),
            _ => None,
        }
    }

    fn symbol(self) -> &'static str {
        match self {
            Self::Celsius => "°C",
            Self::Fahrenheit => "°F",
        }
    }
}

fn parse_unit(args: &[String]) -> Option<Unit> {
    args.windows(2).find_map(|pair| {
        let flag = pair[0].as_str();
        let value = pair[1].as_str();
        match flag {
            "--unit" | "-u" => Unit::from_arg(value),
            _ => None,
        }
    })
}

fn format_temperature(temperature: f64, unit: Unit) -> String {
    let converted = match unit {
        Unit::Celsius => temperature,
        Unit::Fahrenheit => (temperature * 9.0 / 5.0) + 32.0,
    };
    format!("{converted:.1}{}", unit.symbol())
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct WaybarJSON {
    text: String,
    tooltip: String,
    class: String,
}

#[derive(serde::Deserialize)]
struct WeatherResponse {
    current: CurrentWeather,
    daily: DailyForecast,
}

#[derive(serde::Deserialize)]
struct CurrentWeather {
    temperature_2m: f64,
    apparent_temperature: Option<f64>,
    weather_code: i64,
    relative_humidity_2m: Option<f64>,
    wind_speed_10m: Option<f64>,
    pressure_msl: Option<f64>,
    precipitation_probability: Option<f64>,
}

#[derive(serde::Deserialize)]
struct DailyForecast {
    time: Vec<String>,
    weather_code: Vec<i64>,
    temperature_2m_max: Vec<f64>,
    temperature_2m_min: Vec<f64>,
    precipitation_probability_max: Vec<f64>,
}

/// Build the small fallback payload Waybar can display if something goes sideways.
fn error_payload(message: &str) -> WaybarJSON {
    WaybarJSON {
        text: "???".to_string(),
        tooltip: message.to_string(),
        class: "error".to_string(),
    }
}

fn cache_file_path(file_name: &str) -> Option<PathBuf> {
    let home = env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".cache").join(file_name))
}

fn write_weather_cache(payload: &WaybarJSON) {
    if let Some(path) = cache_file_path(WAYBAR_CACHE_FILE)
        && let Ok(json) = serde_json::to_string(payload) {
            let _ = fs::write(path, json);
        }

    if let Some(path) = cache_file_path(LOCKSCREEN_CACHE_FILE) {
        let _ = fs::write(path, &payload.tooltip);
    }
}

fn read_cached_weather() -> Option<WaybarJSON> {
    let path = cache_file_path(WAYBAR_CACHE_FILE)?;
    let json = fs::read_to_string(path).ok()?;
    serde_json::from_str(&json).ok()
}

fn cached_weather_or_error(message: &str) -> WaybarJSON {
    read_cached_weather().unwrap_or_else(|| error_payload(message))
}

// Nominatim (Reverse Geocoding) Structures
#[derive(serde::Deserialize, Debug)]
#[allow(dead_code)]
struct NominatimAddress {
    city: Option<String>,
    town: Option<String>,
    village: Option<String>,
    state: Option<String>,
    country: Option<String>,
}
#[derive(serde::Deserialize, Debug)]
struct NominatimResponse {
    address: NominatimAddress,
}

/// Performs reverse geocoding to convert coords -> "City, State".
/// Uses OpenStreetMap (Nominatim).
async fn get_city_state(
    client: &reqwest::Client,
    lat: f64,
    lon: f64,
) -> Result<(String, String), Box<dyn std::error::Error>> {
    let response = client
        .get(format!(
            "https://nominatim.openstreetmap.org/reverse?lat={lat}&lon={lon}&format=json&zoom=10"
        ))
        .send()
        .await?
        .json::<NominatimResponse>()
        .await?;
    let addr = response.address;
    // Fallback logic: prefer City -> Town -> Village
    let city = addr
        .city
        .or(addr.town)
        .or(addr.village)
        .unwrap_or_else(|| "Unknown City".to_string());
    let state = addr.state.unwrap_or_default();
    Ok((city, state))
}

async fn get_location(
    connection: &Connection,
    path: OwnedObjectPath,
) -> Result<Location, Box<dyn std::error::Error>> {
    let location = Proxy::new(
        connection,
        "org.freedesktop.GeoClue2",
        path,
        "org.freedesktop.GeoClue2.Location",
    )
    .await?;

    let lat: f64 = location.get_property("Latitude").await?;
    let lon: f64 = location.get_property("Longitude").await?;
    Ok(Location { lat, lon })
}

async fn run_geoclue() -> Result<Location, Box<dyn std::error::Error>> {
    let connection = Connection::system().await?;
    let manager = Proxy::new(
        &connection,
        "org.freedesktop.GeoClue2",
        "/org/freedesktop/GeoClue2/Manager",
        "org.freedesktop.GeoClue2.Manager",
    )
    .await?;

    let client_path: OwnedObjectPath = manager.call("GetClient", &()).await?;
    let client = Proxy::new(
        &connection,
        "org.freedesktop.GeoClue2",
        client_path,
        "org.freedesktop.GeoClue2.Client",
    )
    .await?;

    client
        .set_property("DesktopId", "gnome-datetime-panel")
        .await?;
    client.set_property("RequestedAccuracyLevel", 4u32).await?;

    // Subscribe before Start so we do not miss the first update.
    let mut updates = client.receive_signal("LocationUpdated").await?;
    client.call::<_, _, ()>("Start", &()).await?;

    let location_path: OwnedObjectPath = client.get_property("Location").await?;
    if location_path.as_str() != "/" {
        get_location(&connection, location_path).await
    } else {
        match timeout(Duration::from_secs(5), updates.next()).await {
            Ok(Some(signal)) => {
                let (_old, new): (OwnedObjectPath, OwnedObjectPath) =
                    signal.body().deserialize()?;
                get_location(&connection, new).await
            }
            Ok(None) | Err(_) => Err("Timed out waiting for GeoClue location".into()),
        }
    }
}

async fn fetch_weather(lat: f64, lon: f64) -> Result<WeatherResponse, Box<dyn std::error::Error>> {
    let url = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={lat}&longitude={lon}&current=temperature_2m,apparent_temperature,weather_code,relative_humidity_2m,wind_speed_10m,pressure_msl,precipitation_probability&daily=weather_code,temperature_2m_max,temperature_2m_min,precipitation_probability_max&forecast_days=3&timezone=auto"
    );

    let response = reqwest::get(&url).await?;
    response.error_for_status_ref()?;
    Ok(response.json().await?)
}

fn build_weather_payload(city: String, weather: WeatherResponse, unit: Unit) -> WaybarJSON {
    let temperature = format_temperature(weather.current.temperature_2m, unit);
    let condition = weather_code_to_string(weather.current.weather_code);
    let icon = weather_code_to_icon(weather.current.weather_code);
    let feels_like = weather
        .current
        .apparent_temperature
        .map(|value| format_temperature(value, unit))
        .unwrap_or_else(|| temperature.clone());
    let humidity = weather
        .current
        .relative_humidity_2m
        .map(|value| format!("{value:.0}%"))
        .unwrap_or_else(|| "n/a".to_string());
    let wind = weather
        .current
        .wind_speed_10m
        .map(|value| format!("{value:.0} km/h"))
        .unwrap_or_else(|| "n/a".to_string());
    let pressure = weather
        .current
        .pressure_msl
        .map(|value| format!("{value:.0} hPa"))
        .unwrap_or_else(|| "n/a".to_string());
    let precip = weather
        .current
        .precipitation_probability
        .map(|value| format!("{value:.0}%"))
        .unwrap_or_else(|| "n/a".to_string());

    let mut tooltip_lines = vec![
        format!("Location: {city}"),
        format!("Current: {condition} • {temperature}"),
        format!("Feels like {feels_like} • Humidity {humidity}"),
        format!("Wind: {wind} • Pressure: {pressure}"),
        format!("Precip chance: {precip}"),
    ];

    if !weather.daily.time.is_empty() {
        let high = weather
            .daily
            .temperature_2m_max
            .first()
            .copied()
            .map(|value| format_temperature(value, unit))
            .unwrap_or_else(|| "?".to_string());
        let low = weather
            .daily
            .temperature_2m_min
            .first()
            .copied()
            .map(|value| format_temperature(value, unit))
            .unwrap_or_else(|| "?".to_string());

        tooltip_lines.push(format!("High/Low: {high} / {low}"));

        for idx in 0..weather.daily.time.len().min(3) {
            let day = weather.daily.time[idx].clone();
            let code = weather
                .daily
                .weather_code
                .get(idx)
                .copied()
                .unwrap_or_default();
            let high = weather
                .daily
                .temperature_2m_max
                .get(idx)
                .copied()
                .map(|value| format_temperature(value, unit))
                .unwrap_or_else(|| "?".to_string());
            let low = weather
                .daily
                .temperature_2m_min
                .get(idx)
                .copied()
                .map(|value| format_temperature(value, unit))
                .unwrap_or_else(|| "?".to_string());
            let pop = weather
                .daily
                .precipitation_probability_max
                .get(idx)
                .copied()
                .map(|value| format!("{value:.0}%"))
                .unwrap_or_else(|| "n/a".to_string());
            tooltip_lines.push(format!(
                "{day}: {} {} • Hi {high} • Lo {low} • Rain {pop}",
                weather_code_to_icon(code),
                weather_code_to_string(code)
            ));
        }
    }

    WaybarJSON {
        text: format!("{icon} {temperature}"),
        tooltip: tooltip_lines.join("\n"),
        class: "weather".into(),
    }
}

fn weather_code_to_string(code: i64) -> &'static str {
    match code {
        0 => "Clear Sky",
        1 => "Mainly Clear",
        2 => "Partly Cloudy",
        3 => "Overcast",
        45 => "Fog",
        48 => "Rime Fog",
        51 => "Light Drizzle",
        53 => "Drizzle",
        55 => "Heavy Drizzle",
        56 => "Light Freezing Drizzle",
        57 => "Freezing Drizzle",
        61 => "Light Rain",
        63 => "Rain",
        65 => "Heavy Rain",
        66 => "Light Freezing Rain",
        67 => "Freezing Rain",
        71 => "Light Snow",
        73 => "Snow",
        75 => "Heavy Snow",
        77 => "Snow grains",
        80 => "Light Showers",
        81 => "Showers",
        82 => "Heavy showers",
        85 => "Light Snow Showers",
        86 => "Snow showers",
        95 => "Thunderstorm",
        96 => "Light Thunderstorms With Hail",
        99 => "Thunderstorm With Hail",
        _ => "Unknown",
    }
}

fn weather_code_to_icon(code: i64) -> &'static str {
    match code {
        0 | 1 => "☀️",
        2 => "⛅",
        3 => "☁️",
        45 | 48 => "🌁",
        51 | 53 | 55 => "🌦️",
        56 | 57 => "🥶",
        61 | 63 | 65 => "🌧️",
        66 | 67 => "🧊",
        80..=82 => "☔",
        71 | 73 | 75 | 77 => "❄️",
        85 | 86 => "🌨️",
        95 | 96 | 99 => "⛈️",
        _ => "❓",
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = env::args().collect();
    let unit = parse_unit(&args).unwrap_or_else(|| {
        env::var("WAYBAR_WEATHER_UNIT")
            .ok()
            .and_then(|value| Unit::from_arg(&value))
            .unwrap_or(Unit::Celsius)
    });

    let location = match run_geoclue().await {
        Ok(loc) => loc,
        Err(e) => {
            eprintln!("Error getting location: {e}");
            println!(
                "{}",
                serde_json::to_string(&cached_weather_or_error("Unable to fetch location data"))
                    .unwrap()
            );
            return;
        }
    };

    const NOMINATIM_USER_AGENT: &str =
        "WaybarWeatherScript/2.0-owm (Repo: github.com/Mccalabrese/Arch-multi-session-dot-files)";
    let http_client = reqwest::Client::builder()
        .user_agent(NOMINATIM_USER_AGENT)
        .build()
        .unwrap_or_else(|e| {
            eprintln!("Error creating HTTP client: {e}");
            std::process::exit(1);
        });

    let (geo_res, weather_res) = tokio::join!(
        get_city_state(&http_client, location.lat, location.lon),
        fetch_weather(location.lat, location.lon)
    );

    let (city, state) = geo_res.unwrap_or(("Unknown".to_string(), String::new()));
    let city_label = if state.is_empty() {
        city
    } else {
        format!("{city}, {state}")
    };

    let weather = match weather_res {
        Ok(weather_data) => {
            let payload = build_weather_payload(city_label, weather_data, unit);
            write_weather_cache(&payload);
            payload
        }
        Err(e) => {
            eprintln!("Error getting weather: {e}");
            cached_weather_or_error("Unable to fetch weather data")
        }
    };

    println!("{}", serde_json::to_string(&weather).unwrap());
}

#[cfg(test)]
mod tests {
    use super::{Unit, format_temperature, parse_unit};

    #[test]
    fn parses_supported_units() {
        assert_eq!(
            parse_unit(&["--unit".to_string(), "f".to_string()]),
            Some(Unit::Fahrenheit)
        );
        assert_eq!(
            parse_unit(&["--unit".to_string(), "c".to_string()]),
            Some(Unit::Celsius)
        );
        assert_eq!(
            parse_unit(&["--unit".to_string(), "F".to_string()]),
            Some(Unit::Fahrenheit)
        );
        assert_eq!(
            parse_unit(&["--unit".to_string(), "C".to_string()]),
            Some(Unit::Celsius)
        );
    }

    #[test]
    fn formats_temperature_for_each_unit() {
        assert_eq!(format_temperature(20.0, Unit::Celsius), "20.0°C");
        assert_eq!(format_temperature(20.0, Unit::Fahrenheit), "68.0°F");
    }
}
