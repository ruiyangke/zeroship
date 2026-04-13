import React, { useState } from "react";
import { useLocation, useNavigate } from "react-router-dom";
import { Bell, Search, ChevronRight, LogOut, User, Settings } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Avatar, AvatarFallback } from "@/components/ui/avatar";
import { Badge } from "@/components/ui/badge";
import {
  DropdownMenu, DropdownMenuContent, DropdownMenuItem,
  DropdownMenuLabel, DropdownMenuSeparator, DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { cn } from "@/lib/utils";

const routeLabels: Record<string, string> = {
  "": "Dashboard",
  employees: "Employees",
  departments: "Departments",
  recruitment: "Recruitment",
  attendance: "Attendance",
  leave: "Leave",
  payroll: "Payroll",
  performance: "Performance",
  training: "Training",
  benefits: "Benefits",
  documents: "Documents",
  settings: "Settings",
  notifications: "Notifications",
};

function Breadcrumbs() {
  const location = useLocation();
  const parts = location.pathname.split("/").filter(Boolean);

  const crumbs = [
    { label: "Dashboard", href: "/" },
    ...parts.map((part, i) => ({
      label: routeLabels[part] ?? part.charAt(0).toUpperCase() + part.slice(1),
      href: "/" + parts.slice(0, i + 1).join("/"),
    })),
  ];

  if (crumbs.length === 1) return (
    <span className="text-sm font-medium">Dashboard</span>
  );

  return (
    <nav className="flex items-center gap-1 text-sm">
      {crumbs.map((crumb, i) => (
        <React.Fragment key={crumb.href}>
          {i > 0 && <ChevronRight className="h-3 w-3 text-muted-foreground" />}
          <span className={cn(
            i === crumbs.length - 1
              ? "font-medium text-foreground"
              : "text-muted-foreground"
          )}>
            {crumb.label}
          </span>
        </React.Fragment>
      ))}
    </nav>
  );
}

interface HeaderProps {
  unreadCount?: number;
}

export function Header({ unreadCount = 0 }: HeaderProps) {
  const navigate = useNavigate();
  const [searchOpen, setSearchOpen] = useState(false);

  return (
    <header className="h-14 border-b flex items-center gap-4 px-6 bg-background flex-shrink-0">
      <div className="flex-1">
        <Breadcrumbs />
      </div>

      {/* Search */}
      <div className="relative">
        {searchOpen ? (
          <Input
            autoFocus
            placeholder="Search employees..."
            className="w-64 h-8 text-sm"
            onBlur={() => setSearchOpen(false)}
            onKeyDown={(e) => {
              if (e.key === "Escape") setSearchOpen(false);
              if (e.key === "Enter") {
                navigate(`/employees?q=${(e.target as HTMLInputElement).value}`);
                setSearchOpen(false);
              }
            }}
          />
        ) : (
          <Button
            variant="ghost"
            size="sm"
            className="text-muted-foreground"
            onClick={() => setSearchOpen(true)}
          >
            <Search className="h-4 w-4" />
          </Button>
        )}
      </div>

      {/* Notifications */}
      <Button
        variant="ghost"
        size="sm"
        className="relative text-muted-foreground"
        onClick={() => navigate("/notifications")}
      >
        <Bell className="h-4 w-4" />
        {unreadCount > 0 && (
          <Badge
            variant="destructive"
            className="absolute -top-0.5 -right-0.5 h-4 min-w-4 px-1 text-xs flex items-center justify-center"
          >
            {unreadCount > 99 ? "99+" : unreadCount}
          </Badge>
        )}
      </Button>

      {/* User menu */}
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button variant="ghost" size="sm" className="gap-2">
            <Avatar className="h-7 w-7">
              <AvatarFallback className="text-xs bg-primary text-primary-foreground">
                AD
              </AvatarFallback>
            </Avatar>
            <span className="text-sm font-medium">Admin</span>
          </Button>
        </DropdownMenuTrigger>
        <DropdownMenuContent align="end" className="w-48">
          <DropdownMenuLabel className="text-xs text-muted-foreground">
            admin@company.com
          </DropdownMenuLabel>
          <DropdownMenuSeparator />
          <DropdownMenuItem onClick={() => navigate("/settings")}>
            <Settings className="mr-2 h-4 w-4" />
            Settings
          </DropdownMenuItem>
          <DropdownMenuSeparator />
          <DropdownMenuItem className="text-destructive">
            <LogOut className="mr-2 h-4 w-4" />
            Sign out
          </DropdownMenuItem>
        </DropdownMenuContent>
      </DropdownMenu>
    </header>
  );
}
