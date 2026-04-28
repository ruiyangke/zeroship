import { useState } from "react";

export default function App() {
  const [count, setCount] = useState(0);

  return (
    <main className="min-h-screen flex flex-col items-center justify-center p-8">
      <div className="max-w-xl w-full text-center">
        <h1 className="text-4xl font-bold tracking-tight mb-3">
          your app starts here
        </h1>
        <p className="text-zinc-600 mb-10">
          a fresh React + Vite + Tailwind project running on{" "}
          <span className="font-mono text-sm bg-zinc-200 px-1.5 py-0.5 rounded">
            zeroship
          </span>
          . tell the agent on the left what you want to build.
        </p>

        <button
          onClick={() => setCount((c) => c + 1)}
          className="inline-flex items-center justify-center rounded-md bg-zinc-900 text-white px-5 py-2.5 text-sm font-medium hover:bg-zinc-800 transition-colors"
        >
          count is {count}
        </button>

        <p className="mt-10 text-xs text-zinc-400 font-mono">
          edit <span className="text-zinc-600">src/App.tsx</span> to get started
        </p>
      </div>
    </main>
  );
}
