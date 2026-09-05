package controller

import (
	"context"
	"fmt"

	appsv1 "k8s.io/api/apps/v1"
	corev1 "k8s.io/api/core/v1"
	apiequality "k8s.io/apimachinery/pkg/api/equality"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/util/intstr"
	ctrl "sigs.k8s.io/controller-runtime"
	"sigs.k8s.io/controller-runtime/pkg/client"
	"sigs.k8s.io/controller-runtime/pkg/controller/controllerutil"

	platformv1alpha1 "xscope.dev/xscope/api/v1alpha1"
)

type ModelDeploymentReconciler struct {
	client.Client
	Scheme    *runtime.Scheme
	ClusterID string
	Region    string
}

// +kubebuilder:rbac:groups=platform.xscope.io,resources=modeldeployments,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups=platform.xscope.io,resources=modeldeployments/status,verbs=get;update;patch
// +kubebuilder:rbac:groups=apps,resources=deployments,verbs=get;list;watch;create;update;patch;delete
// +kubebuilder:rbac:groups="",resources=services,verbs=get;list;watch;create;update;patch;delete
func (r *ModelDeploymentReconciler) Reconcile(ctx context.Context, request ctrl.Request) (ctrl.Result, error) {
	var model platformv1alpha1.ModelDeployment
	if err := r.Get(ctx, request.NamespacedName, &model); err != nil {
		return ctrl.Result{}, client.IgnoreNotFound(err)
	}

	labels := map[string]string{
		"app.kubernetes.io/name":       model.Name,
		"app.kubernetes.io/component":  "model-runtime",
		"app.kubernetes.io/managed-by": "xscope-operator",
	}
	if r.ClusterID != "" {
		labels["platform.xscope.io/cluster-id"] = r.ClusterID
	}
	if r.Region != "" {
		labels["platform.xscope.io/region"] = r.Region
	}
	deployment := &appsv1.Deployment{ObjectMeta: metav1.ObjectMeta{Name: model.Name, Namespace: model.Namespace}}
	if _, err := controllerutil.CreateOrUpdate(ctx, r.Client, deployment, func() error {
		if err := controllerutil.SetControllerReference(&model, deployment, r.Scheme); err != nil {
			return err
		}
		deployment.Spec.Replicas = &model.Spec.Replicas
		deployment.Spec.Selector = &metav1.LabelSelector{MatchLabels: labels}
		deployment.Spec.Template.ObjectMeta.Labels = labels
		port := model.Spec.Runtime.Port
		if port == 0 {
			port = 8000
		}
		deployment.Spec.Template.Spec = corev1.PodSpec{
			NodeSelector: model.Spec.NodeSelector,
			Tolerations:  model.Spec.Tolerations,
			Containers: []corev1.Container{{
				Name: "runtime", Image: model.Spec.Runtime.Image, Args: model.Spec.Runtime.Arguments,
				Ports:     []corev1.ContainerPort{{Name: "http", ContainerPort: port}},
				Resources: model.Spec.Resources,
				Env: []corev1.EnvVar{
					{Name: "XSCOPE_MODEL_ID", Value: model.Spec.Model.ID},
					{Name: "XSCOPE_MODEL_REVISION", Value: model.Spec.Model.Revision},
					{Name: "XSCOPE_MODEL_URI", Value: model.Spec.Model.URI},
					{Name: "XSCOPE_MODEL_CHECKSUM", Value: model.Spec.Model.Checksum},
				},
				ReadinessProbe: &corev1.Probe{ProbeHandler: corev1.ProbeHandler{HTTPGet: &corev1.HTTPGetAction{Path: "/readyz", Port: intstr.FromString("http")}}},
			}},
		}
		return nil
	}); err != nil {
		return ctrl.Result{}, err
	}

	service := &corev1.Service{ObjectMeta: metav1.ObjectMeta{Name: model.Name, Namespace: model.Namespace}}
	if _, err := controllerutil.CreateOrUpdate(ctx, r.Client, service, func() error {
		if err := controllerutil.SetControllerReference(&model, service, r.Scheme); err != nil {
			return err
		}
		port := model.Spec.Runtime.Port
		if port == 0 {
			port = 8000
		}
		service.Spec.Selector = labels
		service.Spec.Ports = []corev1.ServicePort{{Name: "http", Port: port, TargetPort: intstr.FromString("http")}}
		return nil
	}); err != nil {
		return ctrl.Result{}, err
	}

	desiredStatus := platformv1alpha1.ModelDeploymentStatus{
		ObservedGeneration: model.Generation,
		ReadyReplicas:      deployment.Status.ReadyReplicas,
		Endpoint:           fmt.Sprintf("http://%s.%s.svc:%d", service.Name, service.Namespace, service.Spec.Ports[0].Port),
		ClusterID:          r.ClusterID,
		Region:             r.Region,
		Conditions:         model.Status.Conditions,
	}
	if !apiequality.Semantic.DeepEqual(model.Status, desiredStatus) {
		model.Status = desiredStatus
		if err := r.Status().Update(ctx, &model); err != nil && !apierrors.IsConflict(err) {
			return ctrl.Result{}, err
		}
	}
	return ctrl.Result{}, nil
}

func (r *ModelDeploymentReconciler) SetupWithManager(manager ctrl.Manager) error {
	return ctrl.NewControllerManagedBy(manager).
		For(&platformv1alpha1.ModelDeployment{}).
		Owns(&appsv1.Deployment{}).
		Owns(&corev1.Service{}).
		Complete(r)
}
