const PARK_SLOPE = Object.freeze({
  name: "Park Slope, Brooklyn, NY",
  latitude: 40.6728,
  longitude: -73.9778,
});

function binding() {
  const url = process.env.PAY_BINDING_WEATHER_URL;
  const capability = process.env.PAY_BINDING_WEATHER_CAPABILITY;
  if (!url || !capability) {
    throw new Error("weather data binding is not configured");
  }
  return { url: `${url}/latest`, capability };
}

async function bindingRequest(method, value) {
  const { url, capability } = binding();
  const response = await fetch(url, {
    method,
    headers: {
      "content-type": "application/json",
      "x-pay-data-capability": capability,
    },
    body: value === undefined ? undefined : JSON.stringify(value),
  });
  if (!response.ok) {
    throw new Error(`managed data request failed with ${response.status}`);
  }
  return response.json();
}

async function refreshWeather() {
  const query = new URLSearchParams({
    latitude: String(PARK_SLOPE.latitude),
    longitude: String(PARK_SLOPE.longitude),
    current:
      "temperature_2m,relative_humidity_2m,apparent_temperature,precipitation,weather_code,wind_speed_10m",
    timezone: "America/New_York",
  });
  const response = await fetch(`https://api.open-meteo.com/v1/forecast?${query}`);
  if (!response.ok) {
    throw new Error(`weather provider request failed with ${response.status}`);
  }
  const weather = await response.json();
  const snapshot = {
    location: PARK_SLOPE,
    observed_at: weather.current?.time,
    fetched_at: new Date().toISOString(),
    timezone: weather.timezone,
    units: weather.current_units,
    current: weather.current,
    source: "open-meteo",
  };
  await bindingRequest("PUT", snapshot);
  return snapshot;
}

exports.worker = async (request, response) => {
  try {
    if (request.method === "POST" && request.path === "/refresh") {
      response.status(200).json(await refreshWeather());
      return;
    }
    if (request.method === "GET" && ["/", "/latest"].includes(request.path)) {
      const document = await bindingRequest("GET");
      response.set("cache-control", "public, max-age=60");
      response.status(200).json(document.value);
      return;
    }
    response.status(404).json({ error: "not_found" });
  } catch (error) {
    console.error(error instanceof Error ? error.message : "worker failed");
    response.status(502).json({ error: "worker_failed" });
  }
};

exports.refreshWeather = refreshWeather;
