import { NavLink } from "react-router-dom";
import { type ReactNode } from "react";
import { LayoutDashboard, Box, Plus, LogOut, Bot } from "lucide-react";
import { cn } from "@/lib/utils";
import { Separator } from "@/components/ui/separator";

interface LayoutProps {
  children: ReactNode;
  onLogout: () => void;
}

const navItems = [
  { to: "/", label: "overview", icon: LayoutDashboard, end: true },
  { to: "/apps", label: "apps", icon: Box, end: false },
  { to: "/apps/new", label: "create app", icon: Plus, end: false },
  { to: "/ai", label: "ai agent", icon: Bot, end: false },
];

export default function Layout({ children, onLogout }: LayoutProps) {
  return (
    <div className="flex h-screen">
      <aside className="w-[220px] min-w-[220px] bg-card border-r border-border flex flex-col">
        <div className="px-4 py-5">
          <h1 className="text-sm font-bold tracking-[0.15em] uppercase text-primary">
            zeroship
          </h1>
          <div className="text-[11px] text-muted-foreground tracking-[0.05em] mt-0.5">
            control plane
          </div>
        </div>
        <Separator />
        <nav className="flex-1 py-2">
          <ul className="list-none">
            {navItems.map(({ to, label, icon: Icon, end }) => (
              <li key={to}>
                <NavLink
                  to={to}
                  end={end}
                  className={({ isActive }) =>
                    cn(
                      "flex items-center gap-2.5 px-4 py-2.5 text-[13px] tracking-[0.03em] border-l-2 border-transparent transition-all duration-150",
                      isActive
                        ? "text-primary border-l-primary bg-primary/5"
                        : "text-muted-foreground hover:text-foreground hover:bg-white/[0.03]"
                    )
                  }
                >
                  <Icon className="h-3.5 w-3.5" />
                  {label}
                </NavLink>
              </li>
            ))}
          </ul>
        </nav>
        <Separator />
        <div className="px-4 py-3">
          <button
            onClick={onLogout}
            className="flex items-center gap-2 text-[11px] uppercase tracking-[0.05em] text-destructive bg-transparent border-none font-mono cursor-pointer transition-opacity duration-150 hover:opacity-70"
          >
            <LogOut className="h-3 w-3" />
            logout
          </button>
        </div>
      </aside>
      <main className="flex-1 overflow-y-auto p-8">
        {children}
      </main>
    </div>
  );
}
