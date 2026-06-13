//! Location service for Google Maps API integration.
//!
//! This module provides geocoding and nearby search functionality using Google Maps APIs.
//! Results are cached in SQLite to minimize API calls.

use crate::types::{CoordinateSystem, Location, LocationCacheEntry};
use serde::Deserialize;
use thiserror::Error;

/// Errors that can occur during location service operations.
#[derive(Error, Debug)]
pub enum LocationServiceError {
    #[error("HTTP request failed: {0}")]
    HttpError(#[from] reqwest::Error),

    #[error("Google API error: {status} - {message}")]
    ApiError { status: String, message: String },

    #[error("No results found for query: {0}")]
    NoResults(String),

    #[error("Invalid API key")]
    InvalidApiKey,

    #[error("Rate limit exceeded")]
    RateLimitExceeded,

    #[error("Storage error: {0}")]
    StorageError(String),
}

/// Google Maps Geocoding API response structures.
#[derive(Debug, Deserialize)]
pub struct GeocodingResponse {
    pub status: String,
    pub results: Vec<GeocodingResult>,
    pub error_message: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GeocodingResult {
    pub formatted_address: String,
    pub geometry: Geometry,
    pub place_id: String,
    pub types: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Geometry {
    pub location: LatLng,
}

#[derive(Debug, Deserialize)]
pub struct LatLng {
    pub lat: f64,
    pub lng: f64,
}

/// Google Maps Places Nearby Search API response structures.
#[derive(Debug, Deserialize)]
pub struct NearbySearchResponse {
    pub status: String,
    pub results: Vec<NearbyResult>,
    pub error_message: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct NearbyResult {
    pub name: String,
    pub geometry: Geometry,
    pub place_id: String,
    pub types: Vec<String>,
    pub vicinity: Option<String>,
}

/// Location service for querying Google Maps APIs.
pub struct LocationService {
    api_key: String,
    client: reqwest::Client,
}

impl LocationService {
    /// Create a new LocationService with the given Google Maps API key.
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            client: reqwest::Client::new(),
        }
    }

    /// Query a location by name/address using Google Geocoding API.
    ///
    /// Returns the first matching result as a Location with coordinates.
    /// The coordinates are in WGS-84 (Google Maps standard).
    pub async fn geocode(&self, query: &str) -> Result<GeocodingResult, LocationServiceError> {
        let url = format!(
            "https://maps.googleapis.com/maps/api/geocode/json?address={}&key={}",
            urlencoding::encode(query),
            self.api_key
        );

        let response: GeocodingResponse = self.client.get(&url).send().await?.json().await?;

        match response.status.as_str() {
            "OK" => response
                .results
                .into_iter()
                .next()
                .ok_or_else(|| LocationServiceError::NoResults(query.to_string())),
            "ZERO_RESULTS" => Err(LocationServiceError::NoResults(query.to_string())),
            "REQUEST_DENIED" => Err(LocationServiceError::InvalidApiKey),
            "OVER_QUERY_LIMIT" => Err(LocationServiceError::RateLimitExceeded),
            status => Err(LocationServiceError::ApiError {
                status: status.to_string(),
                message: response.error_message.unwrap_or_default(),
            }),
        }
    }

    /// Convert a geocoding result to a Location.
    pub fn geocoding_result_to_location(result: &GeocodingResult, query: &str) -> Location {
        Location::Coordinate {
            lat: result.geometry.location.lat,
            lng: result.geometry.location.lng,
            coord_system: CoordinateSystem::Wgs84,
            source_name: Some(query.to_string()),
        }
    }

    /// Convert a geocoding result to a LocationCacheEntry.
    pub fn geocoding_result_to_cache_entry(
        result: &GeocodingResult,
        name: &str,
        is_fuzzy: bool,
    ) -> LocationCacheEntry {
        let now = chrono::Utc::now().timestamp();
        LocationCacheEntry {
            name: name.to_string(),
            is_fuzzy,
            latitude: Some(result.geometry.location.lat),
            longitude: Some(result.geometry.location.lng),
            coord_system: CoordinateSystem::Wgs84,
            place_id: Some(result.place_id.clone()),
            place_type: result.types.first().cloned(),
            formatted_address: Some(result.formatted_address.clone()),
            created_at: now,
            updated_at: now,
        }
    }

    /// Search for nearby places of a specific type using Google Places Nearby Search API.
    ///
    /// This is used for fuzzy location queries like "supermarket" or "restaurant".
    ///
    /// # Arguments
    /// * `lat` - Latitude of the center point (WGS-84)
    /// * `lng` - Longitude of the center point (WGS-84)
    /// * `radius_meters` - Search radius in meters (max 50000)
    /// * `place_type` - Optional place type to filter (e.g., "supermarket", "restaurant")
    /// * `keyword` - Optional keyword to search for
    pub async fn search_nearby(
        &self,
        lat: f64,
        lng: f64,
        radius_meters: u32,
        place_type: Option<&str>,
        keyword: Option<&str>,
    ) -> Result<Vec<NearbyResult>, LocationServiceError> {
        let mut url = format!(
            "https://maps.googleapis.com/maps/api/place/nearbysearch/json?location={},{}&radius={}&key={}",
            lat,
            lng,
            radius_meters.min(50000),
            self.api_key
        );

        if let Some(t) = place_type {
            url.push_str(&format!("&type={}", urlencoding::encode(t)));
        }

        if let Some(k) = keyword {
            url.push_str(&format!("&keyword={}", urlencoding::encode(k)));
        }

        let response: NearbySearchResponse = self.client.get(&url).send().await?.json().await?;

        match response.status.as_str() {
            "OK" => Ok(response.results),
            "ZERO_RESULTS" => Ok(vec![]),
            "REQUEST_DENIED" => Err(LocationServiceError::InvalidApiKey),
            "OVER_QUERY_LIMIT" => Err(LocationServiceError::RateLimitExceeded),
            status => Err(LocationServiceError::ApiError {
                status: status.to_string(),
                message: response.error_message.unwrap_or_default(),
            }),
        }
    }

    /// Check if any place of the given type exists within range of a location.
    ///
    /// This is the core function for fuzzy location matching like
    /// `is_location_in_range(current_location(), "supermarket", 500)`.
    ///
    /// # Arguments
    /// * `lat` - Latitude of the center point (WGS-84)
    /// * `lng` - Longitude of the center point (WGS-84)
    /// * `place_type_or_keyword` - Place type or keyword to search for
    /// * `radius_meters` - Search radius in meters
    pub async fn is_place_nearby(
        &self,
        lat: f64,
        lng: f64,
        place_type_or_keyword: &str,
        radius_meters: u32,
    ) -> Result<bool, LocationServiceError> {
        // Try as place type first, then as keyword
        let results = self
            .search_nearby(lat, lng, radius_meters, Some(place_type_or_keyword), None)
            .await?;

        if !results.is_empty() {
            return Ok(true);
        }

        // If no results with type, try keyword search
        let results = self
            .search_nearby(lat, lng, radius_meters, None, Some(place_type_or_keyword))
            .await?;

        Ok(!results.is_empty())
    }
}

/// Calculate the distance between two coordinates using the Haversine formula.
///
/// Returns distance in meters.
pub fn haversine_distance(lat1: f64, lng1: f64, lat2: f64, lng2: f64) -> f64 {
    const EARTH_RADIUS_METERS: f64 = 6_371_000.0;

    let lat1_rad = lat1.to_radians();
    let lat2_rad = lat2.to_radians();
    let delta_lat = (lat2 - lat1).to_radians();
    let delta_lng = (lng2 - lng1).to_radians();

    let a = (delta_lat / 2.0).sin().powi(2)
        + lat1_rad.cos() * lat2_rad.cos() * (delta_lng / 2.0).sin().powi(2);

    let c = 2.0 * a.sqrt().asin();

    EARTH_RADIUS_METERS * c
}

/// Check if two coordinates are within a certain distance of each other.
pub fn is_within_range(lat1: f64, lng1: f64, lat2: f64, lng2: f64, radius_meters: f64) -> bool {
    haversine_distance(lat1, lng1, lat2, lng2) <= radius_meters
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_haversine_distance() {
        // Test with known distance: Tokyo Station to Shibuya Station (~6.6 km)
        let tokyo_lat = 35.6812;
        let tokyo_lng = 139.7671;
        let shibuya_lat = 35.6580;
        let shibuya_lng = 139.7016;

        let distance = haversine_distance(tokyo_lat, tokyo_lng, shibuya_lat, shibuya_lng);

        // Should be approximately 6.6 km
        assert!(distance > 6000.0 && distance < 7000.0);
    }

    #[test]
    fn test_is_within_range() {
        let lat1 = 35.6812;
        let lng1 = 139.7671;
        let lat2 = 35.6815;
        let lng2 = 139.7675;

        // These points are very close (~50m)
        assert!(is_within_range(lat1, lng1, lat2, lng2, 100.0));
        assert!(!is_within_range(lat1, lng1, lat2, lng2, 10.0));
    }

    #[test]
    fn test_same_point_distance() {
        let lat = 35.6812;
        let lng = 139.7671;

        let distance = haversine_distance(lat, lng, lat, lng);
        assert!(distance < 0.001); // Should be essentially 0
    }
}
