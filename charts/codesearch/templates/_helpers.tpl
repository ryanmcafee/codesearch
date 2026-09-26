{{- define "codesearch.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "codesearch.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{- define "codesearch.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "codesearch.labels" -}}
helm.sh/chart: {{ include "codesearch.chart" . }}
{{ include "codesearch.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- with .Values.commonLabels }}
{{ toYaml . }}
{{- end }}
{{- end }}

{{- define "codesearch.selectorLabels" -}}
app.kubernetes.io/name: {{ include "codesearch.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "codesearch.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "codesearch.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{- define "codesearch.image" -}}
{{- $tag := default .Chart.AppVersion .Values.image.tag -}}
{{- if .Values.image.digest -}}
{{- printf "%s@%s" .Values.image.repository .Values.image.digest -}}
{{- else -}}
{{- printf "%s:%s" .Values.image.repository $tag -}}
{{- end -}}
{{- end }}

{{- define "codesearch.apiKeySecretName" -}}
{{- default (printf "%s-api-key" (include "codesearch.fullname" .)) .Values.auth.existingSecret }}
{{- end }}

{{- define "codesearch.apiKeySecretKey" -}}
{{- .Values.auth.existingSecretKey | default "api-key" }}
{{- end }}

{{- define "codesearch.dataClaimName" -}}
{{- default (printf "%s-data" (include "codesearch.fullname" .)) .Values.persistence.data.existingClaim }}
{{- end }}

{{- define "codesearch.reposClaimName" -}}
{{- default (printf "%s-repos" (include "codesearch.fullname" .)) .Values.persistence.repos.existingClaim }}
{{- end }}

{{/*
Host header values the MCP endpoint accepts: the Service's DNS names, loopback
(kubectl port-forward), Ingress hosts and allowedHosts. Entries match any port.
*/}}
{{- define "codesearch.allowedHosts" -}}
{{- $svc := include "codesearch.fullname" . -}}
{{- $ns := .Release.Namespace -}}
{{- $hosts := list $svc (printf "%s.%s" $svc $ns) (printf "%s.%s.svc" $svc $ns) (printf "%s.%s.svc.%s" $svc $ns .Values.clusterDomain) "localhost" "127.0.0.1" "::1" -}}
{{- if .Values.ingress.enabled -}}
{{- range .Values.ingress.hosts -}}
{{- $hosts = append $hosts .host -}}
{{- end -}}
{{- end -}}
{{- $hosts = concat $hosts .Values.allowedHosts -}}
{{- join "," (uniq $hosts) -}}
{{- end }}

{{- define "codesearch.allowedRoots" -}}
{{- join ";" (uniq (prepend .Values.allowedRoots .Values.persistence.repos.mountPath)) -}}
{{- end }}
