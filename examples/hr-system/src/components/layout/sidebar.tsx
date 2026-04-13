import React from "react";
import { NavLink, useLocation } from "react-router-dom";
import {
  LayoutDashboard, Users, Building2, Briefcase, Clock, CalendarDays,
  DollarSign, Star, GraduationCap, Heart, FileText, Settings,
  Bell, ChevronLeft, ChevronRight, Building,
} from "lucide-react";
import { cn } from "@/lib/utils";
import { Button } from "@/components/ui/button";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";

const navItems = [
  { label: "Dashboard",    href: "/",               icon: LayoutDashboard },
  { label: "Employees",    href: "/employees",       icon: Users },
  { label: "Departments",  href: "/departments",     icon: Building2 },
  { label: "Recruitment",  href: "/recruitment",     icon: Briefcase },
  { label: "Attendance",   href: "/attendance",      icon: Clock },
  { label: "Leave",        href: "/leave",           icon: CalendarDays },
  { label: "Payroll",      href: "/payroll",         icon: DollarSign },
  { label: "Performance",  href: "/performance",     icon: Star },
  { label: "Training",     href: "/training",        icon: GraduationCap },
  { label: "Benefits",     href: "/benefits",        icon: Heart },
  { label: "Documents",    href: "/documents",       icon: FileText },
  { label: "Settings",     href: "/settings",        icon: Settings },
  { label: "Notifications",href: "/notifications",   icon: Bell },
];

interface SidebarProps {
  collapsed: boolean;
  onToggle: () => void;
}

export function Sidebar({ collapsed, onToggle }: SidebarProps) {
  const location = useLocation();

  function isActive(href: string) {
    if (href === "/") return location.pathname === "/";
    return location.pathname.startsWith(href);
  }

  return (
    <aside
      className={cn(
        "flex flex-col h-full bg-sidebar border-r border-sidebar-border transition-all duration-200",
        collapsed ? "w-16" : "w-60"
      )}
    >
      {/* Logo */}
      <div className="flex items-center gap-3 h-14 px-4 border-b border-sidebar-border flex-shrink-0">
        <div className="flex items-center justify-center w-7 h-7 rounded-md bg-sidebar-primary flex-shrink-0">
          <Building className="h-4 w-4 text-sidebar-primary-foreground" />
        </div>
        {!collapsed && (
          <span className="font-semibold text-sidebar-foreground truncate">
            HRCore
          </span>
        )}
      </div>

      {/* Nav */}
      <ScrollArea className="flex-1 py-2">
        <nav className="space-y-0.5 px-2">
          {navItems.map(({ label, href, icon: Icon }) => {
            const active = isActive(href);
            const item = (
              <NavLink
                key={href}
                to={href}
                className={cn(
                  "flex items-center gap-3 rounded-md px-2.5 py-2 text-sm font-medium transition-colors",
                  "text-sidebar-foreground/70 hover:text-sidebar-foreground hover:bg-sidebar-accent",
                  active && "bg-sidebar-accent text-sidebar-accent-foreground"
                )}
              >
                <Icon className="h-4 w-4 flex-shrink-0" />
                {!collapsed && <span className="truncate">{label}</span>}
              </NavLink>
            );

            if (collapsed) {
              return (
                <Tooltip key={href} delayDuration={0}>
                  <TooltipTrigger asChild>{item}</TooltipTrigger>
                  <TooltipContent side="right">{label}</TooltipContent>
                </Tooltip>
              );
            }

            return item;
          })}
        </nav>
      </ScrollArea>

      {/* Collapse toggle */}
      <div className="p-2 border-t border-sidebar-border flex-shrink-0">
        <Button
          variant="ghost"
          size="sm"
          className="w-full justify-center text-sidebar-foreground/50 hover:text-sidebar-foreground"
          onClick={onToggle}
        >
          {collapsed ? <ChevronRight className="h-4 w-4" /> : <ChevronLeft className="h-4 w-4" />}
          {!collapsed && <span className="ml-2 text-xs">Collapse</span>}
        </Button>
      </div>
    </aside>
  );
}
