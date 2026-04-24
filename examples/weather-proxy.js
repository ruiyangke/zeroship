// Weather Proxy — outbound fetch + JSON parse.
//
// Demonstrates the two idioms side by side:
//   - `"use server"` named async functions: auto-wired as RPC via
//     `POST /_rpc/<export>` (see `current` / `forecast` below).
//   - `export default { fetch }` HTTP handler: serves GET /:city as
//     a convenience.
//
// The platform routes `/_rpc/*` first, falls through to `fetch` for
// everything else. Mixing is explicitly supported.

"use server";

export async function current(city) {
  if (!city) throw new Error("city is required");
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

export async function forecast(city, days = 3) {
  if (!city) throw new Error("city is required");
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
