// Weather Proxy — fetches from wttr.in
// Tests: fetch, async/await, JSON parsing, error handling

export async function current(city) {
    if (!city) throw new Error("city is required");
    const resp = await fetch(`https://wttr.in/${encodeURIComponent(city)}?format=j1`);
    if (!resp.ok) throw new Error(`Weather API returned ${resp.status}`);
    const data = await resp.json();
    const current = data.current_condition[0];
    return {
        city,
        temp_c: current.temp_C,
        temp_f: current.temp_F,
        description: current.weatherDesc[0].value,
        humidity: current.humidity,
        wind_kmph: current.windspeedKmph,
    };
}

export async function forecast(city, days) {
    if (!city) throw new Error("city is required");
    days = days || 3;
    const resp = await fetch(`https://wttr.in/${encodeURIComponent(city)}?format=j1`);
    if (!resp.ok) throw new Error(`Weather API returned ${resp.status}`);
    const data = await resp.json();
    return data.weather.slice(0, days).map(day => ({
        date: day.date,
        maxTemp: day.maxtempC,
        minTemp: day.mintempC,
        description: day.hourly[4].weatherDesc[0].value,
    }));
}
