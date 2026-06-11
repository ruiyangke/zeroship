// Weather Proxy — outbound fetch + JSON parse.
//
// Demonstrates `export default { rpc: { ... } }` (dict-shape RPC) which
// works under raw `zeroship serve` without a Vite build step.
//
// Each handler receives its entire input as a single value (the wire
// payload), so multi-parameter calls use an object `{ city, days }`.
//
// NOTE: The `"use server"` directive is a Vite build-time transform
// (`@zeroship/vite-plugin`) that auto-wires named exports as RPC via
// `POST /__zeroship/v1/<export>`. It does NOT run in raw `zeroship serve`
// — raw serve requires the explicit `export default { rpc: { ... } }`
// shape. See ISS-55.
//
// Hit endpoints with:
//   curl -X POST http://localhost:3000/__zeroship/v1/current \
//        -H 'content-type: application/json' \
//        -d '{"json":"London"}'
//   curl -X POST http://localhost:3000/__zeroship/v1/forecast \
//        -H 'content-type: application/json' \
//        -d '{"json":{"city":"London","days":3}}'

async function current(input) {
  // Accept either a plain string city or { city } object.
  const city = (typeof input === "string") ? input : input?.city;
  if (!city) throw Object.assign(new Error("city is required"), { status: 400, code: "INVALID_ARGUMENT" });
  const resp = await fetch(`https://wttr.in/${encodeURIComponent(city)}?format=j1`);
  if (!resp.ok) throw new Error(`Weather API returned ${resp.status}`);
  const data = await resp.json();
  const cur = data.current_condition[0];
  return {
    city,
    temp_c: cur.temp_C,
    temp_f: cur.temp_F,
    description: cur.weatherDesc[0].value,
    humidity: cur.humidity,
    wind_kmph: cur.windspeedKmph,
  };
}

async function forecast(input) {
  // Accept either a plain string city or { city, days } object.
  const city = (typeof input === "string") ? input : input?.city;
  const days = (typeof input === "object" && input?.days != null) ? input.days : 3;
  if (!city) throw Object.assign(new Error("city is required"), { status: 400, code: "INVALID_ARGUMENT" });
  const resp = await fetch(`https://wttr.in/${encodeURIComponent(city)}?format=j1`);
  if (!resp.ok) throw new Error(`Weather API returned ${resp.status}`);
  const data = await resp.json();
  return data.weather.slice(0, days).map((day) => ({
    date: day.date,
    maxTemp: day.maxtempC,
    minTemp: day.mintempC,
    description: day.hourly[4].weatherDesc[0].value,
  }));
}

// Dict-shape RPC: works under raw `zeroship serve` (no build step needed).
// Each key becomes a `POST /__zeroship/v1/<key>` endpoint.
export default {
  rpc: { current, forecast },
};
