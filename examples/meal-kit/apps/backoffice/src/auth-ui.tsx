import type { ReactNode } from "react";
import { useLingui } from "@lingui/react";
import { msg } from "@lingui/core/macro";
import { Button, Empty, ErrorState, Loading } from "@gather/meal-kit/components/shared";
import { useGather } from "./state";
export function SignIn({ children }: { children: ReactNode }) {
  const { session, sessionError, refreshSession, act, login } = useGather();
  const { _: t } = useLingui();
  if (sessionError) return <ErrorState error={sessionError} retry={() => { void act(refreshSession); }} />;
  if (!session) return <Loading />;
  if (!session.user) return <Empty title={t(msg`Staff workspace`)} text={t(msg`This area is available to authorized operators. Sign in with your staff account.`)}><Button onClick={() => act(login)}>{t(msg`Sign in to continue`)}</Button></Empty>;
  return children;
}
