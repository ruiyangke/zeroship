# HR System — Full Feature Spec

## Models

### Core
- `employees` — id, first_name, last_name, email, phone, department_id, position_id, manager_id, hire_date, salary, status (active/on_leave/terminated), skills[], avatar_url, emergency_contact_name, emergency_contact_phone, address, date_of_birth
- `departments` — id, name, code, manager_id, budget, headcount, parent_department_id
- `positions` — id, title, department_id, level (junior/mid/senior/lead/director/vp/c-level), salary_min, salary_max, is_open, description

### Recruitment
- `job_postings` — id, position_id, title, description, requirements, status (draft/open/closed), posted_date, closing_date
- `applicants` — id, job_posting_id, name, email, phone, resume_url, stage (applied/screening/interview/offer/hired/rejected), rating, notes, applied_date
- `interviews` — id, applicant_id, interviewer_id, scheduled_at, duration_minutes, type (phone/video/onsite), status (scheduled/completed/cancelled), feedback, rating

### Time & Attendance
- `timesheets` — id, employee_id, date, clock_in, clock_out, hours_worked, overtime_hours, status (draft/submitted/approved), notes
- `work_schedules` — id, employee_id, day_of_week, start_time, end_time, is_remote

### Leave
- `leave_requests` — id, employee_id, type (vacation/sick/personal/parental/bereavement), start_date, end_date, days, reason, status (pending/approved/denied/cancelled), approved_by, approved_at
- `leave_balances` — id, employee_id, year, vacation_total, vacation_used, sick_total, sick_used, personal_total, personal_used
- `holidays` — id, name, date, is_recurring

### Payroll
- `payroll_runs` — id, period, run_date, status (draft/processing/completed), total_gross, total_net, total_deductions, processed_by
- `payslips` — id, payroll_run_id, employee_id, base_salary, overtime_pay, bonus, deductions_tax, deductions_benefits, deductions_other, net_pay, status (pending/paid)

### Performance
- `reviews` — id, employee_id, reviewer_id, period, cycle (quarterly/annual), rating (1-5), strengths, improvements, goals, status (draft/submitted/acknowledged)
- `goals` — id, employee_id, title, description, target_date, status (active/completed/cancelled), progress (0-100), category (performance/development/project)
- `feedback` — id, from_employee_id, to_employee_id, type (praise/constructive), message, is_anonymous

### Training
- `courses` — id, title, description, category, duration_hours, is_mandatory, max_participants
- `enrollments` — id, course_id, employee_id, status (enrolled/in_progress/completed/dropped), enrolled_at, completed_at, score
- `certifications` — id, employee_id, name, issuer, issue_date, expiry_date, credential_url

### Compensation & Benefits
- `compensation_history` — id, employee_id, effective_date, salary, change_type (hire/promotion/adjustment/annual), change_reason, approved_by
- `benefits_plans` — id, name, type (health/dental/vision/life/retirement), provider, monthly_cost_employee, monthly_cost_employer
- `benefits_enrollments` — id, employee_id, plan_id, start_date, end_date, status (active/cancelled)
- `expense_claims` — id, employee_id, description, amount, category (travel/meals/equipment/other), receipt_url, status (submitted/approved/rejected/reimbursed), submitted_at, approved_by

### Compliance & Documents
- `documents` — id, employee_id, type (contract/id/certification/policy/other), name, file_url, uploaded_at, expires_at
- `audit_log` — id, actor_id, action (create/update/delete), entity_type, entity_id, changes_json, timestamp
- `policies` — id, title, content, version, effective_date, category (handbook/conduct/safety/privacy)

### Notifications
- `notifications` — id, employee_id, type (leave_approved/review_due/payroll_ready/course_reminder/general), title, message, is_read, created_at, link

## API Endpoints (by module)

### Employee Management (12 endpoints)
- `createEmployee` — hire with full profile
- `getEmployees` — list with filters (department, status, search)
- `getEmployee` — by id with full profile
- `updateEmployee` — update profile fields
- `terminateEmployee` — set status, update headcount
- `getOrgChart` — recursive manager→reports tree
- `getDirectReports` — immediate team for a manager
- `searchEmployees` — full-text search by name/email/skills
- `addSkill` / `removeSkill` — manage skill tags
- `getEmployeesByDepartment` — filtered list
- `getEmployeeHistory` — compensation + position changes over time

### Department (6 endpoints)
- `createDepartment` / `getDepartments` / `getDepartment`
- `updateDepartment` / `deleteDepartment`
- `getDepartmentTree` — hierarchical org structure

### Position (6 endpoints)
- `createPosition` / `getPositions` / `getOpenPositions`
- `updatePosition` / `closePosition`
- `getPositionsByDepartment`

### Recruitment (10 endpoints)
- `createJobPosting` / `getJobPostings` / `getJobPosting`
- `publishJobPosting` / `closeJobPosting`
- `applyToJob` / `getApplicants` / `updateApplicantStage`
- `scheduleInterview` / `submitInterviewFeedback`

### Time & Attendance (8 endpoints)
- `clockIn` / `clockOut`
- `submitTimesheet` / `approveTimesheet`
- `getTimesheets` — by employee + date range
- `getWorkSchedule` / `updateWorkSchedule`
- `getOvertimeReport`

### Leave (10 endpoints)
- `requestLeave` / `approveLeave` / `denyLeave` / `cancelLeave`
- `getLeaveRequests` — by employee or pending
- `getLeaveBalance` — current year accruals
- `getLeaveCalendar` — team view
- `getHolidays` / `createHoliday`
- `getLeaveReport` — utilization stats

### Payroll (8 endpoints)
- `createPayrollRun` / `processPayroll` / `finalizePayroll`
- `getPayrollRuns` / `getPayslip`
- `getPayslipsByEmployee`
- `getPayrollSummary` — totals per period
- `exportPayroll` — CSV format

### Performance (10 endpoints)
- `createReview` / `submitReview` / `acknowledgeReview`
- `getReviewsForEmployee` / `getPendingReviews`
- `createGoal` / `updateGoalProgress` / `getGoals`
- `giveFeedback` / `getFeedbackForEmployee`

### Training (8 endpoints)
- `createCourse` / `getCourses`
- `enrollInCourse` / `completeCourse` / `getEnrollments`
- `addCertification` / `getCertifications` / `getExpiringCertifications`

### Compensation & Benefits (8 endpoints)
- `adjustSalary` / `getCompensationHistory`
- `getBenefitsPlans` / `enrollInBenefit` / `getBenefitsEnrollments`
- `submitExpense` / `approveExpense` / `getExpenses`

### Documents & Compliance (6 endpoints)
- `uploadDocument` / `getDocuments` / `getExpiringDocuments`
- `getPolicies` / `getAuditLog`
- `acknowledgePolicy`

### Notifications (4 endpoints)
- `getNotifications` / `markAsRead` / `markAllAsRead`
- `getUnreadCount`

### Analytics (10 endpoints)
- `getDashboard` — summary cards
- `getHeadcountByDepartment` — breakdown
- `getSalaryDistribution` — min/max/avg per dept
- `getAttritionReport` — turnover rate
- `getLeaveUtilization` — days used vs available
- `getReviewStats` — avg ratings per period
- `getTimeToHire` — avg days per position
- `getDiversityMetrics` — department composition
- `getCostPerEmployee` — total compensation breakdown
- `getMonthlyTrends` — headcount/attrition over time

## Frontend Pages

1. **Dashboard** — stat cards, charts, recent activity
2. **Employee Directory** — search, table, profiles
3. **Employee Profile** — full detail, tabs (info/reviews/leave/payroll/documents)
4. **Departments** — org tree, headcount cards
5. **Recruitment** — job postings, applicant pipeline kanban
6. **Time & Attendance** — timesheet grid, approvals
7. **Leave Management** — request form, calendar, approvals
8. **Payroll** — run wizard, payslip table, summary
9. **Performance** — review forms, goal tracker, feedback wall
10. **Training** — course catalog, enrollment tracker
11. **Benefits** — plan selector, enrollment status
12. **Documents** — file manager, expiry alerts
13. **Settings** — policies, holidays, company info
14. **Notifications** — inbox
