import React, { useEffect, useState } from "react";
import { toast } from "sonner";
import { FileText, Plus, Loader2, Download, AlertTriangle } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Badge } from "@/components/ui/badge";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter,
} from "@/components/ui/dialog";
import {
  Select, SelectContent, SelectItem, SelectTrigger, SelectValue,
} from "@/components/ui/select";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { PageHeader } from "@/components/shared/page-header";
import { EmptyState } from "@/components/shared/empty-state";
import { TableSkeleton } from "@/components/shared/loading";
import { DataTable, Column } from "@/components/shared/data-table";
import { getDocuments, uploadDocument, getExpiringDocuments, getPolicies } from "@/index";

const MY_EMPLOYEE_ID = 1;
const DOC_TYPES = ["contract", "id", "certification", "policy", "other"];

export default function DocumentsPage() {
  const [documents, setDocuments] = useState<any[]>([]);
  const [expiring, setExpiring] = useState<any[]>([]);
  const [policies, setPolicies] = useState<any[]>([]);
  const [loading, setLoading] = useState(true);
  const [open, setOpen] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [form, setForm] = useState({
    name: "",
    type: "",
    file_url: "",
    expires_at: "",
  });

  async function load() {
    setLoading(true);
    try {
      const [docRes, expRes, polRes] = await Promise.all([
        getDocuments(MY_EMPLOYEE_ID) as any,
        getExpiringDocuments(60) as any,
        getPolicies() as any,
      ]);
      setDocuments(docRes.data || []);
      setExpiring(expRes.data || []);
      setPolicies(polRes.data || []);
    } finally {
      setLoading(false);
    }
  }

  useEffect(() => { load(); }, []);

  function set(key: string, value: string) {
    setForm((f) => ({ ...f, [key]: value }));
  }

  async function handleUpload() {
    if (!form.name || !form.type || !form.file_url) {
      toast.error("Name, type, and file URL are required");
      return;
    }
    setSubmitting(true);
    try {
      const r = await uploadDocument(
        MY_EMPLOYEE_ID,
        form.type,
        form.name,
        form.file_url,
        form.expires_at ? new Date(form.expires_at).getTime() : undefined
      ) as any;
      if (r.error) { toast.error("Failed to upload document"); return; }
      toast.success("Document added");
      setOpen(false);
      setForm({ name: "", type: "", file_url: "", expires_at: "" });
      load();
    } finally {
      setSubmitting(false);
    }
  }

  const columns: Column<any>[] = [
    {
      key: "name",
      header: "Document",
      cell: (row) => (
        <div className="flex items-center gap-2">
          <FileText className="h-4 w-4 text-muted-foreground flex-shrink-0" />
          <span className="font-medium text-sm">{row.name}</span>
        </div>
      ),
    },
    {
      key: "type",
      header: "Type",
      cell: (row) => (
        <Badge variant="outline" className="capitalize">{row.type}</Badge>
      ),
    },
    {
      key: "uploaded_at",
      header: "Uploaded",
      sortable: true,
      cell: (row) =>
        row.uploaded_at ? new Date(row.uploaded_at).toLocaleDateString() : "—",
    },
    {
      key: "expires_at",
      header: "Expires",
      cell: (row) => {
        if (!row.expires_at) return "—";
        const isExpiringSoon =
          row.expires_at - Date.now() < 60 * 86_400_000;
        return (
          <span className={isExpiringSoon ? "text-amber-500 font-medium" : ""}>
            {new Date(row.expires_at).toLocaleDateString()}
          </span>
        );
      },
    },
    {
      key: "actions",
      header: "",
      cell: (row) => (
        <Button
          size="sm"
          variant="ghost"
          className="h-7"
          asChild
        >
          <a href={row.file_url} target="_blank" rel="noopener noreferrer">
            <Download className="h-3.5 w-3.5" />
          </a>
        </Button>
      ),
    },
  ];

  return (
    <div>
      <PageHeader
        title="Documents"
        description="Manage employee documents and policies"
        actions={
          <Button onClick={() => setOpen(true)}>
            <Plus className="mr-2 h-4 w-4" />
            Add Document
          </Button>
        }
      />

      {expiring.length > 0 && (
        <div className="flex items-start gap-3 p-4 rounded-lg bg-amber-500/10 border border-amber-500/20 mb-6">
          <AlertTriangle className="h-4 w-4 text-amber-500 mt-0.5 flex-shrink-0" />
          <div>
            <p className="text-sm font-medium text-amber-600">
              {expiring.length} document{expiring.length !== 1 ? "s" : ""} expiring within 60 days
            </p>
            <p className="text-xs text-muted-foreground mt-0.5">
              Review and renew these documents to stay compliant
            </p>
          </div>
        </div>
      )}

      <Tabs defaultValue="documents">
        <TabsList className="mb-4">
          <TabsTrigger value="documents">Documents</TabsTrigger>
          <TabsTrigger value="policies">Policies</TabsTrigger>
        </TabsList>

        <TabsContent value="documents">
          {loading ? (
            <TableSkeleton rows={5} cols={4} />
          ) : (
            <DataTable
              data={documents}
              columns={columns}
              searchable
              searchPlaceholder="Search documents..."
              searchKeys={["name", "type"]}
              emptyState={
                <EmptyState
                  icon={FileText}
                  title="No documents"
                  description="Upload documents to keep employee records organized"
                  action={{ label: "Add Document", onClick: () => setOpen(true) }}
                />
              }
            />
          )}
        </TabsContent>

        <TabsContent value="policies">
          {loading ? (
            <TableSkeleton rows={3} cols={3} />
          ) : policies.length === 0 ? (
            <EmptyState
              icon={FileText}
              title="No policies"
              description="Company policies will appear here"
            />
          ) : (
            <div className="space-y-2">
              {policies.map((p: any) => (
                <div key={p._id} className="flex items-center gap-4 p-3 rounded-lg border">
                  <FileText className="h-4 w-4 text-muted-foreground flex-shrink-0" />
                  <div className="flex-1">
                    <div className="text-sm font-medium">{p.title}</div>
                    <div className="text-xs text-muted-foreground">
                      v{p.version} &bull; {p.category}
                      {p.effective_date && ` &bull; Effective ${new Date(p.effective_date).toLocaleDateString()}`}
                    </div>
                  </div>
                  <Badge variant="outline" className="capitalize">{p.category}</Badge>
                </div>
              ))}
            </div>
          )}
        </TabsContent>
      </Tabs>

      <Dialog open={open} onOpenChange={setOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>Add Document</DialogTitle>
          </DialogHeader>
          <div className="space-y-4 py-2">
            <div className="space-y-1.5">
              <Label>Document Name *</Label>
              <Input
                value={form.name}
                onChange={(e) => set("name", e.target.value)}
                placeholder="Employment Contract 2026"
              />
            </div>
            <div className="space-y-1.5">
              <Label>Type *</Label>
              <Select value={form.type} onValueChange={(v) => set("type", v)}>
                <SelectTrigger>
                  <SelectValue placeholder="Select type" />
                </SelectTrigger>
                <SelectContent>
                  {DOC_TYPES.map((t) => (
                    <SelectItem key={t} value={t} className="capitalize">{t}</SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>
            <div className="space-y-1.5">
              <Label>File URL *</Label>
              <Input
                value={form.file_url}
                onChange={(e) => set("file_url", e.target.value)}
                placeholder="https://storage.example.com/doc.pdf"
              />
            </div>
            <div className="space-y-1.5">
              <Label>Expiry Date (optional)</Label>
              <Input
                type="date"
                value={form.expires_at}
                onChange={(e) => set("expires_at", e.target.value)}
              />
            </div>
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setOpen(false)}>Cancel</Button>
            <Button onClick={handleUpload} disabled={submitting}>
              {submitting && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
              Add
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
