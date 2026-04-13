import React, { useEffect, useState } from "react";
import { toast } from "sonner";
import { Bell, CheckCheck, Loader2 } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent } from "@/components/ui/card";
import { cn } from "@/lib/utils";
import { PageHeader } from "@/components/shared/page-header";
import { EmptyState } from "@/components/shared/empty-state";
import { TableSkeleton } from "@/components/shared/loading";
import { getNotifications, markAsRead, markAllAsRead } from "@/index";

const MY_EMPLOYEE_ID = 1;

const TYPE_ICONS: Record<string, string> = {
  leave_approved: "✓",
  review_due: "⭐",
  payroll_ready: "$",
  course_reminder: "📚",
  general: "•",
};

export default function NotificationsPage() {
  const [notifications, setNotifications] = useState<any[]>([]);
  const [loading, setLoading] = useState(true);
  const [markingAll, setMarkingAll] = useState(false);

  const unreadCount = notifications.filter((n) => !n.is_read).length;

  async function load() {
    setLoading(true);
    try {
      const r = await getNotifications(MY_EMPLOYEE_ID) as any;
      setNotifications(r.data || []);
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => { load(); }, []);

  async function handleMarkRead(id: number) {
    await markAsRead(id);
    setNotifications((prev) =>
      prev.map((n) => n._id === id ? { ...n, is_read: true } : n)
    );
  }

  async function handleMarkAllRead() {
    setMarkingAll(true);
    try {
      await markAllAsRead(MY_EMPLOYEE_ID);
      setNotifications((prev) => prev.map((n) => ({ ...n, is_read: true })));
      toast.success("All notifications marked as read");
    } finally {
      setMarkingAll(false);
    }
  }

  return (
    <div>
      <PageHeader
        title="Notifications"
        description={
          unreadCount > 0
            ? `${unreadCount} unread notification${unreadCount !== 1 ? "s" : ""}`
            : "You're all caught up"
        }
        actions={
          unreadCount > 0 ? (
            <Button variant="outline" size="sm" onClick={handleMarkAllRead} disabled={markingAll}>
              {markingAll ? (
                <Loader2 className="mr-2 h-4 w-4 animate-spin" />
              ) : (
                <CheckCheck className="mr-2 h-4 w-4" />
              )}
              Mark all read
            </Button>
          ) : undefined
        }
      />

      {loading ? (
        <TableSkeleton rows={5} cols={1} />
      ) : notifications.length === 0 ? (
        <EmptyState
          icon={Bell}
          title="No notifications"
          description="You're all caught up! Notifications will appear here as things happen."
        />
      ) : (
        <div className="space-y-2">
          {notifications.map((n: any) => (
            <Card
              key={n._id}
              className={cn(
                "cursor-pointer transition-colors",
                !n.is_read && "border-primary/20 bg-primary/5"
              )}
              onClick={() => !n.is_read && handleMarkRead(n._id)}
            >
              <CardContent className="py-3 px-4">
                <div className="flex items-start gap-3">
                  <div className={cn(
                    "w-8 h-8 rounded-full flex items-center justify-center text-xs font-bold flex-shrink-0 mt-0.5",
                    n.is_read ? "bg-muted text-muted-foreground" : "bg-primary/10 text-primary"
                  )}>
                    {TYPE_ICONS[n.type] || "•"}
                  </div>
                  <div className="flex-1 min-w-0">
                    <div className="flex items-center gap-2">
                      <span className={cn(
                        "text-sm",
                        !n.is_read && "font-medium"
                      )}>
                        {n.title}
                      </span>
                      {!n.is_read && (
                        <div className="w-2 h-2 rounded-full bg-primary flex-shrink-0" />
                      )}
                    </div>
                    {n.message && (
                      <p className="text-xs text-muted-foreground mt-0.5 line-clamp-2">
                        {n.message}
                      </p>
                    )}
                  </div>
                  <div className="flex-shrink-0 flex items-center gap-2">
                    <Badge variant="outline" className="text-xs capitalize">
                      {n.type.replace(/_/g, " ")}
                    </Badge>
                    {n.created_at && (
                      <span className="text-xs text-muted-foreground whitespace-nowrap">
                        {new Date(n.created_at).toLocaleDateString()}
                      </span>
                    )}
                  </div>
                </div>
              </CardContent>
            </Card>
          ))}
        </div>
      )}
    </div>
  );
}
