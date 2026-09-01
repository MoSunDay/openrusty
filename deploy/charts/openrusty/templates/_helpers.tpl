{{/* Chart name: chart name or nameOverride. */}}
{{- define "openrusty.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{/* Fully qualified app name: release + name (or fullnameOverride). */}}
{{- define "openrusty.fullname" -}}
{{- if .Values.fullnameOverride -}}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name (include "openrusty.name" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{/* Common labels. */}}
{{- define "openrusty.labels" -}}
app.kubernetes.io/name: {{ include "openrusty.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- end -}}

{{/* Selector labels (immutable part). */}}
{{- define "openrusty.selectorLabels" -}}
app.kubernetes.io/name: {{ include "openrusty.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/* Component resource names (chart-lint asserts these suffixes). */}}
{{- define "openrusty.ingressName" -}}
{{- include "openrusty.fullname" . }}-ingress
{{- end -}}
{{- define "openrusty.egressName" -}}
{{- include "openrusty.fullname" . }}-egress-gateway
{{- end -}}
{{- define "openrusty.demoName" -}}
{{- include "openrusty.fullname" . }}-echo
{{- end -}}

{{/* Image reference shared by every openrusty container. */}}
{{- define "openrusty.image" -}}
{{- printf "%s:%s" .Values.image.repository .Values.image.tag -}}
{{- end -}}
