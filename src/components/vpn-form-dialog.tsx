"use client";

import { invoke } from "@tauri-apps/api/core";
import { emit } from "@tauri-apps/api/event";
import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { toast } from "sonner";
import { LoadingButton } from "@/components/loading-button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { RadioGroup, RadioGroupItem } from "@/components/ui/radio-group";
import { RippleButton } from "@/components/ui/ripple";
import { ScrollArea } from "@/components/ui/scroll-area";
import type { VlessSecurity, VpnConfig } from "@/types";

interface VpnFormDialogProps {
  isOpen: boolean;
  onClose: () => void;
  editingVpn?: VpnConfig | null;
}

type Protocol = "WireGuard" | "Vless";

interface WireGuardFormData {
  name: string;
  privateKey: string;
  address: string;
  dns: string;
  mtu: string;
  peerPublicKey: string;
  peerEndpoint: string;
  allowedIps: string;
  persistentKeepalive: string;
  presharedKey: string;
}

interface VlessFormData {
  name: string;
  address: string;
  port: string;
  uuid: string;
  security: VlessSecurity;
  flow: string;
  fingerprint: string;
  serverName: string;
  publicKey: string;
  shortId: string;
  spiderX: string;
}

const defaultWireGuardForm: WireGuardFormData = {
  name: "",
  privateKey: "",
  address: "",
  dns: "",
  mtu: "",
  peerPublicKey: "",
  peerEndpoint: "",
  allowedIps: "0.0.0.0/0, ::/0",
  persistentKeepalive: "",
  presharedKey: "",
};

const defaultVlessForm: VlessFormData = {
  name: "",
  address: "",
  port: "443",
  uuid: "",
  security: "reality",
  flow: "xtls-rprx-vision",
  fingerprint: "chrome",
  serverName: "",
  publicKey: "",
  shortId: "",
  spiderX: "/",
};

function buildWireGuardConfig(form: WireGuardFormData): string {
  const lines: string[] = ["[Interface]"];
  lines.push(`PrivateKey = ${form.privateKey.trim()}`);
  lines.push(`Address = ${form.address.trim()}`);
  if (form.dns.trim()) lines.push(`DNS = ${form.dns.trim()}`);
  if (form.mtu.trim()) lines.push(`MTU = ${form.mtu.trim()}`);
  lines.push("");
  lines.push("[Peer]");
  lines.push(`PublicKey = ${form.peerPublicKey.trim()}`);
  lines.push(`Endpoint = ${form.peerEndpoint.trim()}`);
  lines.push(`AllowedIPs = ${form.allowedIps.trim()}`);
  if (form.persistentKeepalive.trim())
    lines.push(`PersistentKeepalive = ${form.persistentKeepalive.trim()}`);
  if (form.presharedKey.trim())
    lines.push(`PresharedKey = ${form.presharedKey.trim()}`);
  return lines.join("\n");
}

function buildVlessConfig(form: VlessFormData): string {
  const config: Record<string, unknown> = {
    address: form.address.trim(),
    port: Number(form.port),
    uuid: form.uuid.trim(),
    security: form.security,
    flow: form.flow.trim(),
  };
  if (form.fingerprint.trim()) config.fingerprint = form.fingerprint.trim();
  if (form.serverName.trim()) config.server_name = form.serverName.trim();
  if (form.security === "reality") {
    if (form.publicKey.trim()) config.public_key = form.publicKey.trim();
    if (form.shortId.trim()) config.short_id = form.shortId.trim();
    if (form.spiderX.trim()) config.spider_x = form.spiderX.trim();
  }
  return JSON.stringify(config, null, 2);
}

const FINGERPRINTS = [
  "chrome",
  "firefox",
  "safari",
  "iOS",
  "android",
  "edge",
  "360",
  "random",
];

export function VpnFormDialog({
  isOpen,
  onClose,
  editingVpn,
}: VpnFormDialogProps) {
  const { t } = useTranslation();
  const [isSubmitting, setIsSubmitting] = useState(false);
  const [protocol, setProtocol] = useState<Protocol>("WireGuard");
  const [wireGuardForm, setWireGuardForm] =
    useState<WireGuardFormData>(defaultWireGuardForm);
  const [vlessForm, setVlessForm] = useState<VlessFormData>(defaultVlessForm);

  const resetForms = useCallback(() => {
    setProtocol("WireGuard");
    setWireGuardForm(defaultWireGuardForm);
    setVlessForm(defaultVlessForm);
  }, []);

  useEffect(() => {
    if (isOpen) {
      if (editingVpn) {
        const editingProtocol: Protocol =
          editingVpn.vpn_type === "Vless" ? "Vless" : "WireGuard";
        setProtocol(editingProtocol);
        setWireGuardForm({
          ...defaultWireGuardForm,
          name: editingVpn.name,
        });
        setVlessForm({ ...defaultVlessForm, name: editingVpn.name });
      } else {
        resetForms();
      }
    }
  }, [isOpen, editingVpn, resetForms]);

  const handleClose = useCallback(() => {
    if (!isSubmitting) {
      onClose();
    }
  }, [isSubmitting, onClose]);

  const handleSubmit = useCallback(async () => {
    const name =
      protocol === "WireGuard"
        ? wireGuardForm.name.trim()
        : vlessForm.name.trim();

    if (editingVpn) {
      if (!name) {
        toast.error(t("vpns.form.nameRequired"));
        return;
      }

      setIsSubmitting(true);
      try {
        await invoke("update_vpn_config", {
          vpnId: editingVpn.id,
          name,
        });
        await emit("vpn-configs-changed");
        toast.success(t("vpns.form.updated"));
        onClose();
      } catch (error) {
        const errorMessage =
          error instanceof Error ? error.message : String(error);
        toast.error(t("vpns.form.updateFailed", { error: errorMessage }));
      } finally {
        setIsSubmitting(false);
      }
      return;
    }

    if (!name) {
      toast.error(t("vpns.form.nameRequired"));
      return;
    }

    if (protocol === "WireGuard") {
      const { privateKey, address, peerPublicKey, peerEndpoint } =
        wireGuardForm;
      if (!privateKey.trim()) {
        toast.error(t("vpns.form.privateKeyRequired"));
        return;
      }
      if (!address.trim()) {
        toast.error(t("vpns.form.addressRequired"));
        return;
      }
      if (!peerPublicKey.trim()) {
        toast.error(t("vpns.form.peerPublicKeyRequired"));
        return;
      }
      if (!peerEndpoint.trim()) {
        toast.error(t("vpns.form.peerEndpointRequired"));
        return;
      }
    } else {
      const { address, port, uuid, publicKey, shortId } = vlessForm;
      if (!address.trim()) {
        toast.error(t("vpns.form.vlessAddressRequired"));
        return;
      }
      if (!port.trim() || Number(port) <= 0) {
        toast.error(t("vpns.form.vlessPortRequired"));
        return;
      }
      if (!uuid.trim()) {
        toast.error(t("vpns.form.vlessUuidRequired"));
        return;
      }
      if (vlessForm.security === "reality") {
        if (!publicKey.trim()) {
          toast.error(t("vpns.form.vlessPublicKeyRequired"));
          return;
        }
        if (!shortId.trim()) {
          toast.error(t("vpns.form.vlessShortIdRequired"));
          return;
        }
      }
    }

    setIsSubmitting(true);
    try {
      if (protocol === "WireGuard") {
        const configData = buildWireGuardConfig(wireGuardForm);
        await invoke("create_vpn_config_manual", {
          name,
          vpnType: "WireGuard",
          configData,
        });
      } else {
        const configData = buildVlessConfig(vlessForm);
        await invoke("create_vpn_config_manual", {
          name,
          vpnType: "Vless",
          configData,
        });
      }
      await emit("vpn-configs-changed");
      toast.success(t("vpns.form.created"));
      onClose();
    } catch (error) {
      const errorMessage =
        error instanceof Error ? error.message : String(error);
      toast.error(t("vpns.form.createFailed", { error: errorMessage }));
    } finally {
      setIsSubmitting(false);
    }
  }, [editingVpn, protocol, wireGuardForm, vlessForm, onClose, t]);

  const updateWireGuard = useCallback(
    (field: keyof WireGuardFormData, value: string) => {
      setWireGuardForm((prev) => ({ ...prev, [field]: value }));
    },
    [],
  );

  const updateVless = useCallback(
    (field: keyof VlessFormData, value: string) => {
      setVlessForm((prev) => ({ ...prev, [field]: value }));
    },
    [],
  );

  const dialogTitle = editingVpn
    ? t("vpns.form.titleEdit")
    : t("vpns.form.titleCreate");
  const dialogDescription = editingVpn
    ? t("vpns.form.descEdit")
    : t("vpns.form.descCreate");

  return (
    <Dialog open={isOpen} onOpenChange={handleClose}>
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle>{dialogTitle}</DialogTitle>
          <DialogDescription>{dialogDescription}</DialogDescription>
        </DialogHeader>

        <ScrollArea className="max-h-[min(60vh,calc(100vh-15rem))] overflow-y-auto pr-4">
          <div className="grid gap-4 py-2">
            <div className="grid gap-2">
              <Label htmlFor="vpn-name">{t("vpns.form.name")}</Label>
              <Input
                id="vpn-name"
                value={
                  protocol === "WireGuard" ? wireGuardForm.name : vlessForm.name
                }
                onChange={(e) => {
                  if (protocol === "WireGuard") {
                    updateWireGuard("name", e.target.value);
                  } else {
                    updateVless("name", e.target.value);
                  }
                }}
                placeholder={t("vpns.form.namePlaceholder")}
                disabled={isSubmitting}
              />
            </div>

            {!editingVpn && (
              <div className="grid gap-2">
                <Label>{t("vpns.form.protocol")}</Label>
                <RadioGroup
                  value={protocol}
                  onValueChange={(v) => setProtocol(v as Protocol)}
                  className="flex gap-4"
                >
                  <div className="flex items-center gap-2">
                    <RadioGroupItem value="WireGuard" id="proto-wg" />
                    <Label
                      htmlFor="proto-wg"
                      className="cursor-pointer font-normal"
                    >
                      {t("vpns.form.protocolWireguard")}
                    </Label>
                  </div>
                  <div className="flex items-center gap-2">
                    <RadioGroupItem value="Vless" id="proto-vless" />
                    <Label
                      htmlFor="proto-vless"
                      className="cursor-pointer font-normal"
                    >
                      {t("vpns.form.protocolVless")}
                    </Label>
                  </div>
                </RadioGroup>
              </div>
            )}

            {!editingVpn && protocol === "WireGuard" && (
              <WireGuardFields
                form={wireGuardForm}
                update={updateWireGuard}
                isSubmitting={isSubmitting}
                t={t}
              />
            )}

            {!editingVpn && protocol === "Vless" && (
              <VlessFields
                form={vlessForm}
                update={updateVless}
                isSubmitting={isSubmitting}
                t={t}
              />
            )}
          </div>
        </ScrollArea>

        <DialogFooter>
          <RippleButton
            variant="outline"
            onClick={handleClose}
            disabled={isSubmitting}
          >
            {t("common.buttons.cancel")}
          </RippleButton>
          <LoadingButton isLoading={isSubmitting} onClick={handleSubmit}>
            {editingVpn
              ? t("vpns.form.updateButton")
              : t("vpns.form.createButton")}
          </LoadingButton>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

interface WireGuardFieldsProps {
  form: WireGuardFormData;
  update: (field: keyof WireGuardFormData, value: string) => void;
  isSubmitting: boolean;
  t: (key: string, opts?: Record<string, unknown>) => string;
}

function WireGuardFields({
  form,
  update,
  isSubmitting,
  t,
}: WireGuardFieldsProps) {
  return (
    <>
      <div className="grid gap-2">
        <Label htmlFor="wg-private-key">{t("vpns.form.privateKey")}</Label>
        <Input
          id="wg-private-key"
          value={form.privateKey}
          onChange={(e) => update("privateKey", e.target.value)}
          placeholder={t("vpns.form.privateKeyPlaceholder")}
          disabled={isSubmitting}
        />
      </div>

      <div className="grid gap-2">
        <Label htmlFor="wg-address">{t("vpns.form.address")}</Label>
        <Input
          id="wg-address"
          value={form.address}
          onChange={(e) => update("address", e.target.value)}
          placeholder={t("vpns.form.addressPlaceholder")}
          disabled={isSubmitting}
        />
      </div>

      <div className="grid grid-cols-2 gap-4">
        <div className="grid gap-2">
          <Label htmlFor="wg-dns">{t("vpns.form.dnsOptional")}</Label>
          <Input
            id="wg-dns"
            value={form.dns}
            onChange={(e) => update("dns", e.target.value)}
            placeholder={t("vpns.form.dnsPlaceholder")}
            disabled={isSubmitting}
          />
        </div>

        <div className="grid gap-2">
          <Label htmlFor="wg-mtu">{t("vpns.form.mtuOptional")}</Label>
          <Input
            id="wg-mtu"
            type="number"
            value={form.mtu}
            onChange={(e) => update("mtu", e.target.value)}
            placeholder={t("vpns.form.mtuPlaceholder")}
            disabled={isSubmitting}
          />
        </div>
      </div>

      <div className="grid gap-2">
        <Label htmlFor="wg-peer-public-key">
          {t("vpns.form.peerPublicKey")}
        </Label>
        <Input
          id="wg-peer-public-key"
          value={form.peerPublicKey}
          onChange={(e) => update("peerPublicKey", e.target.value)}
          placeholder={t("vpns.form.peerPublicKeyPlaceholder")}
          disabled={isSubmitting}
        />
      </div>

      <div className="grid gap-2">
        <Label htmlFor="wg-peer-endpoint">{t("vpns.form.peerEndpoint")}</Label>
        <Input
          id="wg-peer-endpoint"
          value={form.peerEndpoint}
          onChange={(e) => update("peerEndpoint", e.target.value)}
          placeholder={t("vpns.form.peerEndpointPlaceholder")}
          disabled={isSubmitting}
        />
      </div>

      <div className="grid gap-2">
        <Label htmlFor="wg-allowed-ips">{t("vpns.form.allowedIps")}</Label>
        <Input
          id="wg-allowed-ips"
          value={form.allowedIps}
          onChange={(e) => update("allowedIps", e.target.value)}
          placeholder={t("vpns.form.allowedIpsPlaceholder")}
          disabled={isSubmitting}
        />
      </div>

      <div className="grid grid-cols-2 gap-4">
        <div className="grid gap-2">
          <Label htmlFor="wg-keepalive">
            {t("vpns.form.keepaliveOptional")}
          </Label>
          <Input
            id="wg-keepalive"
            type="number"
            value={form.persistentKeepalive}
            onChange={(e) => update("persistentKeepalive", e.target.value)}
            placeholder={t("vpns.form.keepalivePlaceholder")}
            disabled={isSubmitting}
          />
        </div>

        <div className="grid gap-2">
          <Label htmlFor="wg-preshared-key">
            {t("vpns.form.presharedKeyOptional")}
          </Label>
          <Input
            id="wg-preshared-key"
            value={form.presharedKey}
            onChange={(e) => update("presharedKey", e.target.value)}
            placeholder={t("vpns.form.presharedKeyPlaceholder")}
            disabled={isSubmitting}
          />
        </div>
      </div>
    </>
  );
}

interface VlessFieldsProps {
  form: VlessFormData;
  update: (field: keyof VlessFormData, value: string) => void;
  isSubmitting: boolean;
  t: (key: string, opts?: Record<string, unknown>) => string;
}

function VlessFields({ form, update, isSubmitting, t }: VlessFieldsProps) {
  return (
    <>
      <div className="grid grid-cols-3 gap-4">
        <div className="col-span-2 grid gap-2">
          <Label htmlFor="vless-address">{t("vpns.form.vlessAddress")}</Label>
          <Input
            id="vless-address"
            value={form.address}
            onChange={(e) => update("address", e.target.value)}
            placeholder={t("vpns.form.vlessAddressPlaceholder")}
            disabled={isSubmitting}
          />
        </div>
        <div className="grid gap-2">
          <Label htmlFor="vless-port">{t("vpns.form.vlessPort")}</Label>
          <Input
            id="vless-port"
            type="number"
            value={form.port}
            onChange={(e) => update("port", e.target.value)}
            placeholder="443"
            disabled={isSubmitting}
          />
        </div>
      </div>

      <div className="grid gap-2">
        <Label htmlFor="vless-uuid">{t("vpns.form.vlessUuid")}</Label>
        <Input
          id="vless-uuid"
          value={form.uuid}
          onChange={(e) => update("uuid", e.target.value)}
          placeholder="00000000-0000-0000-0000-000000000000"
          disabled={isSubmitting}
        />
      </div>

      <div className="grid gap-2">
        <Label>{t("vpns.form.vlessSecurity")}</Label>
        <RadioGroup
          value={form.security}
          onValueChange={(v) => update("security", v as VlessSecurity)}
          className="flex gap-4"
        >
          <div className="flex items-center gap-2">
            <RadioGroupItem value="reality" id="sec-reality" />
            <Label htmlFor="sec-reality" className="cursor-pointer font-normal">
              {t("vpns.form.vlessSecurityReality")}
            </Label>
          </div>
          <div className="flex items-center gap-2">
            <RadioGroupItem value="tls" id="sec-tls" />
            <Label htmlFor="sec-tls" className="cursor-pointer font-normal">
              {t("vpns.form.vlessSecurityTls")}
            </Label>
          </div>
        </RadioGroup>
      </div>

      <div className="grid grid-cols-2 gap-4">
        <div className="grid gap-2">
          <Label htmlFor="vless-flow">{t("vpns.form.vlessFlow")}</Label>
          <Input
            id="vless-flow"
            value={form.flow}
            onChange={(e) => update("flow", e.target.value)}
            placeholder="xtls-rprx-vision"
            disabled={isSubmitting}
          />
        </div>
        <div className="grid gap-2">
          <Label htmlFor="vless-fingerprint">
            {t("vpns.form.vlessFingerprint")}
          </Label>
          <select
            id="vless-fingerprint"
            value={form.fingerprint}
            onChange={(e) => update("fingerprint", e.target.value)}
            disabled={isSubmitting}
            className="flex h-9 w-full rounded-md border border-input bg-transparent px-3 py-1 text-sm shadow-sm focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring"
          >
            <option value="">-</option>
            {FINGERPRINTS.map((fp) => (
              <option key={fp} value={fp}>
                {fp}
              </option>
            ))}
          </select>
        </div>
      </div>

      <div className="grid gap-2">
        <Label htmlFor="vless-sni">{t("vpns.form.vlessServerName")}</Label>
        <Input
          id="vless-sni"
          value={form.serverName}
          onChange={(e) => update("serverName", e.target.value)}
          placeholder={t("vpns.form.vlessServerNamePlaceholder")}
          disabled={isSubmitting}
        />
      </div>

      {form.security === "reality" && (
        <>
          <div className="grid gap-2">
            <Label htmlFor="vless-pbk">{t("vpns.form.vlessPublicKey")}</Label>
            <Input
              id="vless-pbk"
              value={form.publicKey}
              onChange={(e) => update("publicKey", e.target.value)}
              placeholder={t("vpns.form.vlessPublicKeyPlaceholder")}
              disabled={isSubmitting}
            />
          </div>

          <div className="grid grid-cols-2 gap-4">
            <div className="grid gap-2">
              <Label htmlFor="vless-sid">{t("vpns.form.vlessShortId")}</Label>
              <Input
                id="vless-sid"
                value={form.shortId}
                onChange={(e) => update("shortId", e.target.value)}
                placeholder="d9f3ff0ed2b26d77"
                disabled={isSubmitting}
              />
            </div>
            <div className="grid gap-2">
              <Label htmlFor="vless-spx">{t("vpns.form.vlessSpider")}</Label>
              <Input
                id="vless-spx"
                value={form.spiderX}
                onChange={(e) => update("spiderX", e.target.value)}
                placeholder="/"
                disabled={isSubmitting}
              />
            </div>
          </div>
        </>
      )}
    </>
  );
}
