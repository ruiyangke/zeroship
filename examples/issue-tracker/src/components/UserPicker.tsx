// Minimal "find a user" widget: type a fragment of name/handle/email, pick
// from the matches. There is no dedicated typeahead RPC, so this just calls
// users.list({text}) -- which is exactly what it is for.
import { useState } from "react";
import { Button } from "@zeroship/ui";
import { listUsers } from "../api";
import { errorMessage } from "./rpc";
import type { UserRow } from "./types";

export function UserPicker({
  placeholder = "name, handle, or email",
  onPick,
}: {
  placeholder?: string;
  onPick: (user: UserRow) => void;
}) {
  const [text, setText] = useState("");
  const [results, setResults] = useState<UserRow[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [searching, setSearching] = useState(false);

  const search = async () => {
    const query = text.trim();
    if (!query) {
      setResults(null);
      return;
    }
    setSearching(true);
    setError(null);
    try {
      const rows = await listUsers({ text: query, limit: 8 });
      setResults(rows);
    } catch (err) {
      setError(errorMessage(err));
      setResults(null);
    } finally {
      setSearching(false);
    }
  };

  return (
    <div className="user-picker">
      <div className="user-picker-row">
        <input
          value={text}
          placeholder={placeholder}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") {
              e.preventDefault();
              void search();
            }
          }}
        />
        <Button variant="gray" size="small" onClick={() => void search()} disabled={searching}>
          {searching ? "..." : "Find"}
        </Button>
      </div>
      {error ? <p className="field-error">{error}</p> : null}
      {results ? (
        results.length === 0 ? (
          <p className="state-hint small">No matching users.</p>
        ) : (
          <ul className="user-picker-results">
            {results.map((user) => (
              <li key={user.id}>
                <Button variant="gray" size="small"
                  onClick={() => {
                    onPick(user);
                    setText("");
                    setResults(null);
                  }}
                >
                  {user.name} <span className="dim">@{user.handle}</span>
                </Button>
              </li>
            ))}
          </ul>
        )
      ) : null}
    </div>
  );
}
