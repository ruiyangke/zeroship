import {
  createContext,
  forwardRef,
  useContext,
  useId,
  type ComponentPropsWithoutRef,
  type Ref,
} from "react";

const MainIdContext = createContext<string | null>(null);

const AppShellRoot = forwardRef<HTMLDivElement, ComponentPropsWithoutRef<"div">>(
  function AppShellRoot({ className, children, ...props }, ref) {
    const mainId = `${useId()}-main`;

    return (
      <MainIdContext.Provider value={mainId}>
        <div
          {...props}
          ref={ref}
          data-slot="app-shell"
          data-sidebar-open=""
          data-sidebar-side="start"
          className={`isolate flex h-dvh w-full min-w-0 flex-col overflow-hidden bg-canvas font-sans text-base font-normal text-ink leading-[var(--it-leading-snug)]${
            className ? ` ${className}` : ""
          }`}
        >
          <a data-slot="skip-link" href={`#${mainId}`}>
            Skip to main content
          </a>
          {children}
        </div>
      </MainIdContext.Provider>
    );
  },
);

AppShellRoot.displayName = "AppShell";

const AppShellHeader = forwardRef<HTMLElement, ComponentPropsWithoutRef<"header">>(
  function AppShellHeader({ className, ...props }, ref) {
    return (
      <header
        {...props}
        ref={ref}
        data-slot="app-shell-header"
        className={`flex min-h-12 flex-none items-center border-b border-line-strong bg-surface px-4 py-2${
          className ? ` ${className}` : ""
        }`}
      />
    );
  },
);

AppShellHeader.displayName = "AppShell.Header";

const AppShellMain = forwardRef<HTMLElement, ComponentPropsWithoutRef<"main">>(
  function AppShellMain({ className, children, id: _id, ...props }, ref) {
    const mainId = useContext(MainIdContext) ?? undefined;

    return (
      <main
        {...props}
        ref={ref as Ref<HTMLElement>}
        id={mainId}
        data-slot="app-shell-main split-main"
        className={`min-h-0 w-full min-w-0 flex-1 overflow-auto overscroll-contain px-6 pb-8 pt-4${
          className ? ` ${className}` : ""
        }`}
      >
        {children}
      </main>
    );
  },
);

AppShellMain.displayName = "AppShell.Main";

type AppShellComponent = typeof AppShellRoot & {
  Header: typeof AppShellHeader;
  Main: typeof AppShellMain;
};

export const AppShell = AppShellRoot as AppShellComponent;
AppShell.Header = AppShellHeader;
AppShell.Main = AppShellMain;
