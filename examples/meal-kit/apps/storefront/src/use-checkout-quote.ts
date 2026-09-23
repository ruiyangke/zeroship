import { useEffect, useState } from "react";
import { getQuote } from "./api";
import type { Cart, Quote } from "@gather/meal-kit/domain";

export function useCheckoutQuote(cart: Cart) {
  const key = JSON.stringify(cart);
  const [reload, setReload] = useState(0);
  const [result, setResult] = useState<{
    key: string;
    revision: number;
    data?: Quote;
    error?: string;
  }>();
  useEffect(() => {
    let active = true;
    const timer = setTimeout(() => {
      Promise.resolve(getQuote(cart))
        .then((data) => {
          if (active) setResult({ key, revision: reload, data });
        })
        .catch((error) => {
          if (active)
            setResult({
              key,
              revision: reload,
              error: String(error.message ?? error),
            });
        });
    }, 300);
    return () => {
      active = false;
      clearTimeout(timer);
    };
  }, [key, reload]);
  const current =
    result?.key === key && result.revision === reload ? result : undefined;
  return {
    data: current?.data,
    error: current?.error,
    refresh: () => setReload((n) => n + 1),
  };
}
