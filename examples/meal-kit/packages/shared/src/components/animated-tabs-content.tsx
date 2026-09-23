import type { ComponentProps } from "react";
import { TabsContent } from "@gather/meal-kit/components/ui/tabs";
import { ContentTransition } from "@gather/meal-kit/components/content-transition";

export function AnimatedTabsContent({
  children,
  value,
  ...props
}: ComponentProps<typeof TabsContent>) {
  return (
    <TabsContent value={value} {...props}>
      <ContentTransition change={String(value)}>{children}</ContentTransition>
    </TabsContent>
  );
}
