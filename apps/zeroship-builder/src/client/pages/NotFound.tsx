import { Link } from "react-router-dom";

export function NotFound() {
  return (
    <main
      data-testid="not-found-page"
      className="min-h-screen bg-paper flex items-center justify-center px-6"
    >
      <div className="max-w-md text-center">
        <div className="label-uc mb-3">Not found</div>
        <h1 className="font-serif italic font-medium text-[34px] leading-tight text-ink mb-3">
          This page does not exist.
        </h1>
        <p className="font-serif text-[15px] text-ink-soft leading-[1.55] mb-6">
          The route may be stale, or the project may have moved.
        </p>
        <Link
          to="/home"
          data-testid="not-found-home"
          className="font-serif italic text-[15px] text-tomato hover:opacity-80"
          style={{ textDecoration: "none" }}
        >
          Back to home
        </Link>
      </div>
    </main>
  );
}
