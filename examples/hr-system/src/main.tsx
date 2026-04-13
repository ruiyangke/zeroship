import React from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter, Routes, Route, Navigate } from "react-router-dom";
import { Toaster } from "@/components/ui/sonner";
import "./index.css";

import { AppLayout } from "@/components/layout/app-layout";
import DashboardPage from "@/pages/dashboard";
import EmployeesPage from "@/pages/employees/index";
import EmployeeProfilePage from "@/pages/employees/profile";
import NewEmployeePage from "@/pages/employees/new";
import DepartmentsPage from "@/pages/departments";
import RecruitmentPage from "@/pages/recruitment/index";
import ApplicantsPage from "@/pages/recruitment/applicants";
import AttendancePage from "@/pages/attendance";
import LeavePage from "@/pages/leave/index";
import LeaveApprovalsPage from "@/pages/leave/approvals";
import PayrollPage from "@/pages/payroll/index";
import RunPayrollPage from "@/pages/payroll/run";
import PerformancePage from "@/pages/performance/index";
import GoalsPage from "@/pages/performance/goals";
import TrainingPage from "@/pages/training";
import BenefitsPage from "@/pages/benefits";
import DocumentsPage from "@/pages/documents";
import SettingsPage from "@/pages/settings";
import NotificationsPage from "@/pages/notifications";

createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <BrowserRouter>
      <Routes>
        <Route element={<AppLayout />}>
          <Route index element={<DashboardPage />} />
          <Route path="employees" element={<EmployeesPage />} />
          <Route path="employees/new" element={<NewEmployeePage />} />
          <Route path="employees/:id" element={<EmployeeProfilePage />} />
          <Route path="departments" element={<DepartmentsPage />} />
          <Route path="recruitment" element={<RecruitmentPage />} />
          <Route path="recruitment/applicants" element={<ApplicantsPage />} />
          <Route path="attendance" element={<AttendancePage />} />
          <Route path="leave" element={<LeavePage />} />
          <Route path="leave/approvals" element={<LeaveApprovalsPage />} />
          <Route path="payroll" element={<PayrollPage />} />
          <Route path="payroll/run" element={<RunPayrollPage />} />
          <Route path="performance" element={<PerformancePage />} />
          <Route path="performance/goals" element={<GoalsPage />} />
          <Route path="training" element={<TrainingPage />} />
          <Route path="benefits" element={<BenefitsPage />} />
          <Route path="documents" element={<DocumentsPage />} />
          <Route path="settings" element={<SettingsPage />} />
          <Route path="notifications" element={<NotificationsPage />} />
          <Route path="*" element={<Navigate to="/" replace />} />
        </Route>
      </Routes>
      <Toaster />
    </BrowserRouter>
  </React.StrictMode>
);
