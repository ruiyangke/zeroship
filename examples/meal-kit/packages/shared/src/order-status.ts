import { msg } from "@lingui/core/macro";
import type { I18n, MessageDescriptor } from "@lingui/core";
export function statusLabel(status: string, t: I18n["_"]) {
  const map: Record<string, MessageDescriptor> = {
    created: msg`Box created`,
    edited: msg`Meals updated`,
    payment_succeeded: msg`Paid`,
    payment_failed: msg`Declined`,
    payment_requires_action: msg`Authentication required`,
    payment_started: msg`Confirming payment`,
    late_payment: msg`Payment under review`,
    late_payment_refunded: msg`Refunded`,
    support_resolved: msg`Support request resolved`,
    confirmed: msg`Confirmed`,
    pending_payment: msg`Payment needs attention`,
    processing: msg`Confirming payment`,
    checkout_expired: msg`Payment time ended`,
    payment_recovery: msg`Payment under review`,
    canceled: msg`Canceled`,
    completed: msg`Delivered`,
    active: msg`Active`,
    paused: msg`Paused`,
    open: msg`Open`,
    resolved: msg`Resolved`,
    succeeded: msg`Paid`,
    failed: msg`Declined`,
    requires_action: msg`Authentication required`,
    unallocated: msg`Awaiting preparation`,
    packing: msg`Packing`,
    packed: msg`Ready for collection`,
    dispatched: msg`On the way`,
    delivered: msg`Delivered`,
    exception: msg`Delivery needs attention`,
  };
  return map[status] ? t(map[status]) : status;
}
